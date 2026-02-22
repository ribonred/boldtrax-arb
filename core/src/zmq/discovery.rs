use crate::types::Exchange;
use redis::AsyncCommands;
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceType {
    Pub,
    Router,
}

impl ServiceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceType::Pub => "pub",
            ServiceType::Router => "router",
        }
    }
}

#[derive(Clone)]
pub struct DiscoveryClient {
    client: redis::Client,
}

impl DiscoveryClient {
    pub fn new(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        Ok(Self { client })
    }

    fn build_key(exchange: Exchange, service_type: ServiceType) -> String {
        format!(
            "discovery:{}:{}",
            format!("{:?}", exchange).to_lowercase(),
            service_type.as_str()
        )
    }

    /// Registers a service endpoint in Redis with a TTL and spawns a heartbeat task to keep it alive.
    pub async fn register_and_heartbeat(
        &self,
        exchange: Exchange,
        service_type: ServiceType,
        endpoint: String,
        ttl_secs: u64,
    ) -> anyhow::Result<()> {
        let key = Self::build_key(exchange, service_type);
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to get Redis connection for ZMQ discovery ({}): {}",
                    key,
                    e
                )
            })?;

        // Initial registration
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(&endpoint)
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to register ZMQ service in Redis ({}): {}", key, e)
            })?;
        info!(key = %key, endpoint = %endpoint, "Registered ZMQ service in Redis");

        // Spawn heartbeat task
        let client_clone = self.client.clone();
        let endpoint_clone = endpoint.clone();
        let heartbeat_interval = Duration::from_secs(ttl_secs / 3);

        tokio::spawn(async move {
            let mut interval = time::interval(heartbeat_interval);
            loop {
                interval.tick().await;
                match client_clone.get_multiplexed_async_connection().await {
                    Ok(mut hb_conn) => {
                        let res: Result<(), redis::RedisError> = redis::cmd("SET")
                            .arg(&key)
                            .arg(&endpoint_clone)
                            .arg("EX")
                            .arg(ttl_secs)
                            .query_async(&mut hb_conn)
                            .await;
                        if let Err(e) = res {
                            error!(key = %key, error = %e, "Failed to send heartbeat to Redis");
                        } else {
                            debug!(key = %key, "Heartbeat sent");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to get Redis connection for heartbeat");
                    }
                }
            }
        });

        Ok(())
    }

    /// Discovers a service endpoint from Redis.
    pub async fn discover_service(
        &self,
        exchange: Exchange,
        service_type: ServiceType,
    ) -> Result<Option<String>, redis::RedisError> {
        let key = Self::build_key(exchange, service_type);
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let endpoint: Option<String> = conn.get(&key).await?;
        Ok(endpoint)
    }
}
