// Copyright 2020 LEXUGE
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

mod client_cache;
mod resp_cache;

use self::client_cache::ClientCache;
use self::resp_cache::{RecordStatus::*, RespCache};
use crate::error::{DrouteError, Result};
use dmatcher::Label;
use futures::future::{select_ok, BoxFuture, FutureExt};
use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::time::{timeout, Duration};
use trust_dns_client::op::Message;
use trust_dns_proto::xfer::dns_handle::DnsHandle;

#[derive(Serialize, Deserialize, Clone)]
pub struct Upstream {
    pub tag: Label,
    pub method: UpstreamKind,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

// Default value for timeout
fn default_timeout() -> u64 {
    5
}

#[derive(Serialize, Deserialize, Clone)]
pub enum UpstreamKind {
    Hybrid(Vec<Label>),
    Https {
        name: String,
        addr: SocketAddr,
        no_sni: bool,
    },
    // Drop TLS support until we figure out how to do without OpenSSL
    // Tls(String, SocketAddr),
    Udp(SocketAddr),
}

pub struct Upstreams {
    upstreams: HashMap<Label, Upstream>,
    client_cache: HashMap<Label, ClientCache>,
    resp_cache: RespCache,
}

impl Upstreams {
    pub async fn new(upstreams: Vec<Upstream>, size: usize) -> Result<Self> {
        let mut r: HashMap<Label, Upstream> = HashMap::new();
        let mut c = HashMap::new();
        for u in upstreams {
            r.insert(u.tag.clone(), u);
        }
        for (k, v) in r.iter() {
            c.insert(k.clone(), ClientCache::new(v).await?);
        }
        let u = Self {
            upstreams: r,
            client_cache: c,
            resp_cache: RespCache::new(size),
        };
        u.hybrid_check()?;
        Ok(u)
    }

    // Check any upstream types
    // tag: current upstream node's tag
    // l: visited tags
    fn hybrid_search(&self, mut l: HashSet<Label>, tag: Label) -> Result<()> {
        if l.contains(&tag) {
            return Err(DrouteError::HybridRecursion(tag));
        } else {
            l.insert(tag.clone());

            if let UpstreamKind::Hybrid(v) = &self
                .upstreams
                .get(&tag)
                .ok_or_else(|| DrouteError::MissingTag(tag.clone()))?
                .method
            {
                // Check if it is empty.
                if v.is_empty() {
                    return Err(DrouteError::EmptyHybrid(tag.clone()));
                }

                // Check if it is recursively defined.
                for t in v {
                    self.hybrid_search(l.clone(), t.clone())?
                }
            }
        }

        Ok(())
    }

    pub fn hybrid_check(&self) -> Result<bool> {
        for (tag, _) in self.upstreams.iter() {
            self.hybrid_search(HashSet::new(), tag.clone())?
        }
        Ok(true)
    }

    pub fn exists(&self, tag: &Label) -> Result<bool> {
        if self.upstreams.contains_key(tag) {
            Ok(true)
        } else {
            Err(DrouteError::MissingTag(tag.clone()))
        }
    }

    // Send the query and handle caching schemes.
    async fn query(
        u: Upstream,
        client_cache: ClientCache,
        resp_cache: RespCache,
        msg: Message,
    ) -> Result<Message> {
        let mut client = client_cache.get_client(&u).await?;
        let r = Message::from(timeout(Duration::from_secs(u.timeout), client.send(msg)).await??);

        // If the response can be obtained sucessfully, we then push back the client to the client cache
        client_cache.return_back(client);
        resp_cache.put(r.clone());
        Ok(r)
    }

    async fn final_resolve(&self, tag: Label, msg: Message) -> Result<Message> {
        let id = msg.id();

        // Check if cache exists
        let mut r = match self.resp_cache.get(&msg) {
            // Cache available within TTL constraints
            Some(Alive(r)) => r,
            Some(Expired(r)) => {
                // Cache records exists, but TTL exceeded.
                // We try to update the cache and return back the outdated value.
                tokio::spawn(Self::query(
                    self.upstreams.get(&tag).unwrap().clone(),
                    self.client_cache.get(&tag).unwrap().clone(),
                    self.resp_cache.clone(),
                    msg.clone(),
                ));
                r
            }
            None => {
                // No cache available, even without TTL constraints. Start a new query.
                // HashMap is created for every single upstream (including Hybrid) when `Upstreams` is created
                Self::query(
                    self.upstreams.get(&tag).unwrap().clone(),
                    self.client_cache.get(&tag).unwrap().clone(),
                    self.resp_cache.clone(),
                    msg.clone(),
                )
                .await?
            }
        };
        r.set_id(id);
        Ok(r)
    }

    // Write out in this way to allow recursion for async functions
    pub fn resolve<'a>(&'a self, tag: Label, msg: Message) -> BoxFuture<'a, Result<Message>> {
        async move {
            let u = self.upstreams.get(&tag).unwrap();
            Ok(match &u.method {
                UpstreamKind::Hybrid(v) => {
                    let v = v.iter().map(|t| self.resolve(t.clone(), msg.clone()));
                    let (r, _) = select_ok(v.clone()).await?;
                    r
                }
                _ => self.final_resolve(tag, msg).await?,
            })
        }
        .boxed()
    }
}
