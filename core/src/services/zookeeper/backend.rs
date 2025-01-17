// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::collections::HashMap;
use zookeeper_client as zk;

use crate::raw::adapters::kv;
use crate::Scheme;
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::raw::build_rooted_abs_path;
use crate::Builder;
use crate::Error;
use crate::ErrorKind;
use crate::*;

use std::fmt::Debug;
use std::fmt::Formatter;

use substring::Substring;

use log::warn;

const DEFAULT_ZOOKEEPER_ENDPOINT: &str = "127.0.0.1:2181";
/// The scheme for zookeeper authentication
/// currently we do not support sasl authentication
const ZOOKEEPER_AUTH_SCHEME: &str = "digest";

/// Zookeeper backend builder
#[derive(Clone, Default)]
pub struct ZookeeperBuilder {
    /// network address of the Zookeeper service
    /// Default: 127.0.0.1:2181
    endpoint: Option<String>,
    /// the user to connect to zookeeper service, default None
    username: Option<String>,
    /// the password file of the user to connect to zookeeper service, default None
    password: Option<String>,
}

impl ZookeeperBuilder {
    /// Set the endpoint of zookeeper service
    pub fn endpoint(&mut self, endpoint: &str) -> &mut Self {
        if !endpoint.is_empty() {
            self.endpoint = Some(endpoint.to_string());
        }
        self
    }

    /// Set the username of zookeeper service
    pub fn username(&mut self, username: &str) -> &mut Self {
        if !username.is_empty() {
            self.username = Some(username.to_string());
        }
        self
    }

    /// Specify the password of zookeeper service
    pub fn password(&mut self, password: &str) -> &mut Self {
        if !password.is_empty() {
            self.password = Some(password.to_string());
        }
        self
    }
}

impl Debug for ZookeeperBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("Builder");
        if let Some(endpoint) = self.endpoint.clone() {
            ds.field("endpoint", &endpoint);
        }
        if let Some(username) = self.username.clone() {
            ds.field("username", &username);
        }
        ds.finish()
    }
}

impl Builder for ZookeeperBuilder {
    const SCHEME: Scheme = Scheme::Zookeeper;
    type Accessor = ZookeeperBackend;

    fn from_map(map: HashMap<String, String>) -> Self {
        let mut builder = ZookeeperBuilder::default();

        map.get("endpoint").map(|v| builder.endpoint(v));
        map.get("username").map(|v| builder.username(v));
        map.get("password").map(|v| builder.password(v));

        builder
    }

    fn build(&mut self) -> Result<Self::Accessor> {
        let endpoint = match self.endpoint.clone() {
            None => DEFAULT_ZOOKEEPER_ENDPOINT.to_string(),
            Some(endpoint) => endpoint,
        };
        let (auth, acl) = match (self.username.clone(), self.password.clone()) {
            (Some(username), Some(password)) => {
                let auth = format!("{username}:{password}").as_bytes().to_vec();
                (auth, zk::Acl::creator_all())
            }
            _ => {
                warn!("username and password isn't set, default use `anyone` acl");
                (Vec::<u8>::new(), zk::Acl::anyone_all())
            }
        };

        Ok(ZookeeperBackend::new(ZkAdapter {
            endpoint,
            auth,
            acl,
            client: OnceCell::new(),
        }))
    }
}

/// Backend for Zookeeper service
pub type ZookeeperBackend = kv::Backend<ZkAdapter>;

#[derive(Clone)]
pub struct ZkAdapter {
    endpoint: String,
    auth: Vec<u8>,
    client: OnceCell<zk::Client>,
    acl: &'static [zk::Acl],
}

impl Debug for ZkAdapter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("Adapter");
        ds.field("endpoint", &self.endpoint);
        ds.field("acl", &self.acl);
        ds.finish()
    }
}

impl ZkAdapter {
    async fn get_connection(&self) -> Result<zk::Client> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }
        match zk::Client::connect(&self.endpoint.clone()).await {
            Ok(client) => {
                if !self.auth.is_empty() {
                    client
                        .auth(ZOOKEEPER_AUTH_SCHEME.to_string(), self.auth.clone())
                        .await
                        .map_err(parse_zookeeper_error)?;
                }
                self.client.set(client.clone()).ok();
                Ok(client)
            }
            Err(e) => Err(Error::new(ErrorKind::Unexpected, "error from zookeeper").set_source(e)),
        }
    }

    async fn create_nested_node(&self, path: &str, value: &[u8]) -> Result<()> {
        let mut path = path.to_string();
        if !path.starts_with('/') {
            path = build_rooted_abs_path("/", path.strip_suffix('/').unwrap_or(&path));
        }
        let mut rend = path.len();
        loop {
            let mut subpath = path.substring(0, rend);
            if subpath.is_empty() {
                subpath = "/";
            }
            match self
                .get_connection()
                .await?
                .create(
                    subpath,
                    value,
                    &zk::CreateOptions::new(zk::CreateMode::Persistent, self.acl),
                )
                .await
            {
                Ok(_) => break Ok(()),
                Err(e) => match e {
                    zk::Error::NoNode => {
                        rend = path.substring(0, rend).rfind('/').unwrap();
                    }
                    _ => {
                        break Err(
                            Error::new(ErrorKind::Unexpected, "error from zookeeper").set_source(e)
                        )
                    }
                },
            }
        }?;
        match path.substring(rend + 1, path.len()).find('/') {
            Some(len) => {
                rend = len + rend + 1;
                loop {
                    match self
                        .get_connection()
                        .await?
                        .create(
                            path.substring(0, rend),
                            value,
                            &zk::CreateOptions::new(zk::CreateMode::Persistent, self.acl),
                        )
                        .await
                    {
                        Ok(_) => {
                            if rend == path.len() {
                                return Ok(());
                            } else {
                                match path.substring(rend + 1, path.len()).find('/') {
                                    None => rend = path.len(),
                                    Some(len) => rend = len + rend + 1,
                                }
                            }
                        }
                        Err(e) => {
                            return Err(Error::new(ErrorKind::Unexpected, "error from zookeeper")
                                .set_source(e))
                        }
                    }
                }
            }
            None => Ok(()),
        }
    }
}

#[async_trait]
impl kv::Adapter for ZkAdapter {
    fn metadata(&self) -> kv::Metadata {
        kv::Metadata::new(
            Scheme::Zookeeper,
            "ZooKeeper",
            Capability {
                read: true,
                write: true,
                delete: true,
                ..Default::default()
            },
        )
    }

    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let path = build_rooted_abs_path("/", path.strip_suffix('/').unwrap_or(path));
        match self.get_connection().await?.get_data(&path).await {
            Ok(data) => Ok(Some(data.0)),
            Err(e) => match e {
                zk::Error::NoNode => Ok(None),
                _ => Err(Error::new(ErrorKind::Unexpected, "error from zookeeper").set_source(e)),
            },
        }
    }

    async fn set(&self, path: &str, value: &[u8]) -> Result<()> {
        let path = build_rooted_abs_path("/", path.strip_suffix('/').unwrap_or(path));
        match self
            .get_connection()
            .await?
            .set_data(&path, value, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => match e {
                zk::Error::NoNode => {
                    return self.create_nested_node(&path, value).await;
                }
                _ => Err(Error::new(ErrorKind::Unexpected, "error from zookeeper").set_source(e)),
            },
        }
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let path = build_rooted_abs_path("/", path.strip_suffix('/').unwrap_or(path));
        match self.get_connection().await?.delete(&path, None).await {
            Ok(()) => Ok(()),
            Err(e) => match e {
                zk::Error::NoNode => Ok(()),
                _ => Err(Error::new(ErrorKind::Unexpected, "error from zookeeper").set_source(e)),
            },
        }
    }
}

fn parse_zookeeper_error(e: zk::Error) -> Error {
    Error::new(ErrorKind::Unexpected, "error from zookeeper").set_source(e)
}
