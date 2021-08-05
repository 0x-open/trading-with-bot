use mmb_lib::core::exchanges::common::{Amount, Price};
use mmb_lib::core::exchanges::common::{CurrencyPair, ExchangeAccountId};
use mmb_lib::core::exchanges::general::exchange::Exchange;
use mmb_lib::core::exchanges::general::exchange::RequestResult;
use mmb_lib::core::lifecycle::cancellation_token::CancellationToken;
use mmb_lib::core::orders::order::*;
use mmb_lib::core::orders::pool::OrderRef;
use mmb_lib::core::DateTime;

use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::time::Duration;

use std::sync::Arc;

pub struct Order {
    pub client_order_id: ClientOrderId,
    pub init_time: DateTime,
    pub exchange_account_id: ExchangeAccountId,
    pub currency_pair: CurrencyPair,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub amount: Amount,
    pub execution_type: OrderExecutionType,
    pub reservation_id: Option<ReservationId>,
    pub signal_id: Option<String>,
    pub strategy_name: String,

    pub price: Price,
    pub cancellation_token: CancellationToken,
    timeout: Duration,
}

impl Order {
    pub fn new(
        exchange_account_id: ExchangeAccountId,
        strategy_name: Option<String>,
        cancellation_token: CancellationToken,
    ) -> Order {
        Order {
            client_order_id: ClientOrderId::unique_id(),
            init_time: Utc::now(),
            exchange_account_id: exchange_account_id,
            currency_pair: Order::default_currency_pair(),
            order_type: OrderType::Limit,
            side: OrderSide::Buy,
            amount: Order::default_amount(),
            execution_type: OrderExecutionType::None,
            reservation_id: None,
            signal_id: None,
            strategy_name: strategy_name.unwrap_or("OrderTest".to_owned()),
            price: Order::default_price(),
            cancellation_token: cancellation_token,
            timeout: Duration::from_secs(5),
        }
    }

    pub fn default_currency_pair() -> CurrencyPair {
        CurrencyPair::from_codes("phb".into(), "btc".into())
    }

    pub fn default_amount() -> Decimal {
        dec!(2000)
    }

    pub fn default_price() -> Decimal {
        dec!(0.0000001)
    }

    pub fn make_header(&self) -> Arc<OrderHeader> {
        OrderHeader::new(
            self.client_order_id.clone(),
            self.init_time,
            self.exchange_account_id.clone(),
            self.currency_pair.clone(),
            self.order_type,
            self.side,
            self.amount,
            self.execution_type,
            self.reservation_id.clone(),
            self.signal_id.clone(),
            self.strategy_name.clone(),
        )
    }

    pub async fn create(&self, exchange: Arc<Exchange>) -> Result<OrderRef> {
        let header = self.make_header();
        let to_create = OrderCreating {
            price: self.price,
            header: header.clone(),
        };
        let _ = exchange
            .cancel_all_orders(header.currency_pair.clone())
            .await
            .expect("in test");
        let created_order_fut = exchange.create_order(&to_create, self.cancellation_token.clone());

        let created_order = tokio::select! {
            created_order = created_order_fut => created_order,
            _ = tokio::time::sleep(self.timeout) => panic!("Timeout {} secs is exceeded", self.timeout.as_secs())
        };
        created_order
    }

    pub async fn cancel(&self, order_ref: &OrderRef, exchange: Arc<Exchange>) {
        let header = self.make_header();
        let exchange_order_id = order_ref.exchange_order_id().expect("in test");
        let order_to_cancel = OrderCancelling {
            header: header.clone(),
            exchange_order_id,
        };

        let cancel_outcome = exchange
            .cancel_order(&order_to_cancel, CancellationToken::default())
            .await
            .expect("in test")
            .expect("in test");

        if let RequestResult::Success(gotten_client_order_id) = cancel_outcome.outcome {
            assert_eq!(gotten_client_order_id, self.client_order_id);
        }
    }
}
