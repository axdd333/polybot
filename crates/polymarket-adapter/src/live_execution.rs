use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use polymarket_client_sdk::auth::{LocalSigner, Signer as _};
use polymarket_client_sdk::clob::types::{OrderType, Side as ClobSide, SignatureType};
use polymarket_client_sdk::clob::Client as ClobClient;
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::POLYGON;
use std::str::FromStr;
use tokio::sync::Mutex;
use trading_core::config::{AppProfile, ExecutionMode, LiveProfile, WalletSignatureType};
use trading_core::executor::{ExecutionReport, ExecutionRequest, Executor, PaperExecutor};
use trading_core::market::types::OrderAction;

pub fn build_executor(profile: &AppProfile) -> Result<Box<dyn Executor>> {
    if !matches!(profile.execution.mode, ExecutionMode::Live) {
        return Ok(Box::new(PaperExecutor::new(
            profile.execution.paper.clone(),
        )));
    }

    if !profile.execution.live.enabled {
        bail!("execution.mode is live but execution.live.enabled is false");
    }

    Ok(Box::new(LiveOrderExecutor::new(&profile.execution.live)?))
}

struct LiveOrderExecutor {
    client: Mutex<
        ClobClient<
            polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>,
        >,
    >,
    private_key: String,
}

impl LiveOrderExecutor {
    fn new(config: &LiveProfile) -> Result<Self> {
        let private_key = std::env::var(&config.private_key_env)
            .with_context(|| format!("missing env var {}", config.private_key_env))?;
        let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));
        let mut auth =
            ClobClient::new(&config.clob_host, Default::default())?.authentication_builder(&signer);

        match config.signature_type {
            WalletSignatureType::Eoa => {
                auth = auth.signature_type(SignatureType::Eoa);
            }
            WalletSignatureType::Proxy => {
                auth = auth.signature_type(SignatureType::Proxy);
            }
            WalletSignatureType::GnosisSafe => {
                auth = auth.signature_type(SignatureType::GnosisSafe);
            }
        }

        if let Some(funder) = &config.funder {
            auth = auth.funder(funder.parse::<Address>()?);
        }

        let client = tokio::runtime::Handle::current().block_on(auth.authenticate())?;
        Ok(Self {
            client: Mutex::new(client),
            private_key,
        })
    }

    fn same_order(
        &self,
        pending: &trading_core::state::PendingLiveOrder,
        intent: &trading_core::market::types::OrderIntent,
    ) -> bool {
        pending.action == intent.action
            && (pending.price - intent.price).abs() < 0.0001
            && (pending.qty - intent.qty).abs() < 0.0001
    }

    async fn submit_limit_order(
        &self,
        token_id: &str,
        intent: &trading_core::market::types::OrderIntent,
    ) -> Result<ExecutionReport> {
        let token_id = U256::from_str(token_id)
            .map_err(|err| anyhow!("invalid token id {token_id}: {err}"))?;
        let price = Decimal::from_str_exact(&format!("{:.6}", intent.price))
            .or_else(|_| Decimal::from_str(&intent.price.to_string()))
            .map_err(|err| anyhow!("invalid order price {}: {}", intent.price, err))?;
        let size = Decimal::from_str_exact(&format!("{:.6}", intent.qty))
            .or_else(|_| Decimal::from_str(&intent.qty.to_string()))
            .map_err(|err| anyhow!("invalid order qty {}: {}", intent.qty, err))?;
        let side = match intent.action {
            OrderAction::Buy => ClobSide::Buy,
            OrderAction::Sell => ClobSide::Sell,
        };
        let order_type = if intent.aggressive {
            OrderType::FOK
        } else {
            OrderType::GTC
        };
        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));

        let client = self.client.lock().await;
        let signable = client
            .limit_order()
            .token_id(token_id)
            .size(size)
            .price(price)
            .side(side)
            .order_type(order_type)
            .build()
            .await?;
        let signed = client.sign(&signer, signable).await?;
        let response = client.post_order(signed).await?;
        if !response.success {
            return Ok(ExecutionReport::LiveOrderRejected {
                reason: response
                    .error_msg
                    .unwrap_or_else(|| "order rejected by Polymarket".to_string()),
            });
        }

        Ok(ExecutionReport::LiveOrderAccepted {
            order_id: response.order_id,
            action: intent.action,
            price: intent.price,
            qty: intent.qty,
        })
    }

    async fn cancel_order(&self, order_id: &str) -> Result<ExecutionReport> {
        let client = self.client.lock().await;
        let response = client.cancel_order(order_id).await?;
        if response.canceled.iter().any(|id| id == order_id) {
            Ok(ExecutionReport::LiveOrderCancelled {
                order_id: order_id.to_string(),
            })
        } else {
            Ok(ExecutionReport::LiveOrderRejected {
                reason: response
                    .not_canceled
                    .get(order_id)
                    .cloned()
                    .unwrap_or_else(|| format!("failed to cancel order {order_id}")),
            })
        }
    }
}

#[async_trait]
impl Executor for LiveOrderExecutor {
    async fn execute(&self, request: ExecutionRequest<'_>) -> Result<Vec<ExecutionReport>> {
        if request.surface.intents.len() > 1 {
            return Ok(vec![ExecutionReport::LiveOrderRejected {
                reason: "live executor only supports one active intent per market".to_string(),
            }]);
        }

        let desired = request.surface.intents.first();
        let mut reports = Vec::new();

        match (request.pending, desired) {
            (Some(pending), None) => {
                reports.push(self.cancel_order(&pending.order_id).await?);
            }
            (Some(pending), Some(intent)) if self.same_order(pending, intent) => {}
            (Some(pending), Some(intent)) => {
                reports.push(self.cancel_order(&pending.order_id).await?);
                if matches!(
                    reports.last(),
                    Some(ExecutionReport::LiveOrderCancelled { .. })
                ) {
                    reports.push(self.submit_limit_order(request.token_id, intent).await?);
                }
            }
            (None, Some(intent)) => {
                reports.push(self.submit_limit_order(request.token_id, intent).await?);
            }
            (None, None) => {}
        }

        Ok(reports)
    }
}
