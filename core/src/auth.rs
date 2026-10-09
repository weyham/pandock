use crate::baidu::{BaiduClient, DeviceCodeResponse, TokenResponse};
use std::sync::Arc;
use std::time::Duration;

pub struct DeviceAuth {
    pub client: Arc<BaiduClient>,
    pub info: DeviceCodeResponse,
}
impl DeviceAuth {
    pub async fn start(client: Arc<BaiduClient>) -> Result<Self, String> {
        let info = client.device_code().await?;
        Ok(Self { client, info })
    }
    pub async fn wait(self) -> Result<TokenResponse, String> {
        let mut wait = Duration::from_secs(self.info.interval.unwrap_or(6).max(6));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(self.info.expires_in);
        loop {
            tokio::time::sleep(wait).await;
            if tokio::time::Instant::now() > deadline {
                return Err("设备码已过期".into());
            };
            match self.client.poll_device_token(&self.info.device_code).await {
                Ok(token) => return Ok(token),
                Err(error) if error == "PENDING" => continue,
                Err(error) if error == "SLOW_DOWN" => {
                    wait = (wait + Duration::from_secs(5)).min(Duration::from_secs(60));
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }
}
