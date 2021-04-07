use std::sync::Arc;

use super::{
    currency_pair_metadata::CurrencyPairMetadata, currency_pair_metadata::Round, exchange::Exchange,
};
use crate::core::{
    exchanges::common::Amount, exchanges::common::CurrencyCode, exchanges::common::CurrencyPair,
    exchanges::common::ExchangeAccountId, exchanges::common::Price,
    exchanges::events::AllowedEventSourceType, orders::fill::EventSourceType,
    orders::fill::OrderFill, orders::fill::OrderFillType, orders::order::ClientOrderId,
    orders::order::ExchangeOrderId, orders::order::OrderEventType, orders::order::OrderRole,
    orders::order::OrderSide, orders::order::OrderSnapshot, orders::order::OrderStatus,
    orders::order::OrderType, orders::pool::OrderRef,
};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use log::{error, info, warn};
use parking_lot::RwLock;
use rust_decimal::prelude::Zero;
use rust_decimal_macros::dec;
use uuid::Uuid;

type ArgsToLog = (
    ExchangeAccountId,
    String,
    Option<ClientOrderId>,
    ExchangeOrderId,
    AllowedEventSourceType,
    EventSourceType,
);

#[derive(Debug, Clone)]
pub struct FillEventData {
    pub source_type: EventSourceType,
    pub trade_id: String,
    pub client_order_id: Option<ClientOrderId>,
    pub exchange_order_id: ExchangeOrderId,
    pub fill_price: Price,
    pub fill_amount: Amount,
    pub is_diff: bool,
    pub total_filled_amount: Option<Amount>,
    pub order_role: Option<OrderRole>,
    pub commission_currency_code: Option<CurrencyCode>,
    pub commission_rate: Option<Amount>,
    pub commission_amount: Option<Amount>,
    pub fill_type: OrderFillType,
    pub trade_currency_pair: Option<CurrencyPair>,
    pub order_side: Option<OrderSide>,
    pub order_amount: Option<Amount>,
}

impl Exchange {
    pub fn handle_order_filled(&self, mut event_data: FillEventData) -> Result<()> {
        let args_to_log = (
            self.exchange_account_id.clone(),
            event_data.trade_id.clone(),
            event_data.client_order_id.clone(),
            event_data.exchange_order_id.clone(),
            self.features.allowed_fill_event_source_type,
            event_data.source_type,
        );

        if Self::should_ignore_event(
            self.features.allowed_fill_event_source_type,
            event_data.source_type,
        ) {
            info!("Ignoring fill {:?}", args_to_log);
            return Ok(());
        }

        if event_data.exchange_order_id.as_str().is_empty() {
            Self::log_fill_handling_error_and_propagate(
                "Received HandleOrderFilled with an empty exchangeOrderId",
                &args_to_log,
            )?;
        }

        self.check_based_on_fill_type(&mut event_data, &args_to_log)?;

        match self
            .orders
            .by_exchange_id
            .get(&event_data.exchange_order_id)
        {
            None => {
                info!("Received a fill for not existing order {:?}", &args_to_log);
                // TODO BufferedFillsManager.add_fill()

                if let Some(client_order_id) = event_data.client_order_id {
                    self.raise_order_created(
                        client_order_id,
                        event_data.exchange_order_id,
                        event_data.source_type,
                    );
                }

                return Ok(());
            }
            Some(order) => self.local_order_exist(&mut event_data, &*order),
        }
    }

    fn was_trade_already_received(
        trade_id: &str,
        order_fills: &Vec<OrderFill>,
        order_ref: &OrderRef,
    ) -> bool {
        if !trade_id.is_empty()
            && order_fills.iter().any(|fill| {
                if let Some(fill_trade_id) = fill.trade_id() {
                    return fill_trade_id == &trade_id;
                }

                false
            })
        {
            info!(
                "Trade with {} was received already for order {:?}",
                trade_id, order_ref
            );

            return true;
        }

        false
    }

    fn diff_fill_after_non_diff(
        event_data: &FillEventData,
        order_fills: &Vec<OrderFill>,
        order_ref: &OrderRef,
    ) -> bool {
        if event_data.is_diff && order_fills.iter().any(|fill| !fill.is_diff()) {
            // Most likely we received a trade update (diff), then received a non-diff fill via fallback and then again received a diff trade update
            // It happens when WebSocket is glitchy and we miss update and the problem is we have no idea how to handle diff updates
            // after applying a non-diff one as there's no TradeId, so we have to ignore all the diff updates afterwards
            // relying only on fallbacks
            warn!(
                "Unable to process a diff fill after a non-diff one {:?}",
                order_ref
            );

            return true;
        }

        false
    }

    fn filled_amount_not_less_event_fill(
        event_data: &FillEventData,
        order_filled_amount: Amount,
        order_ref: &OrderRef,
    ) -> bool {
        if !event_data.is_diff && order_filled_amount >= event_data.fill_amount {
            warn!(
                "order.filled_amount is {} >= received fill {}, so non-diff fill for {} {:?} should be ignored",
                order_filled_amount,
                event_data.fill_amount,
                order_ref.client_order_id(),
                order_ref.exchange_order_id(),
            );

            return true;
        }

        false
    }

    // FIXME not fully tested
    fn get_last_fill_data(
        mut event_data: &mut FillEventData,
        currency_pair_metadata: &CurrencyPairMetadata,
        order_fills: &Vec<OrderFill>,
        order_filled_amount: Amount,
        order_ref: &OrderRef,
    ) -> Option<(Price, Amount, Price)> {
        let mut last_fill_amount = event_data.fill_amount;
        let mut last_fill_price = event_data.fill_price;
        let mut last_fill_cost = if !currency_pair_metadata.is_derivative() {
            last_fill_amount * last_fill_price
        } else {
            last_fill_amount / last_fill_price
        };

        if !event_data.is_diff && order_fills.len() > 0 {
            match Self::calculate_cost_diff(&order_fills, &*order_ref, last_fill_cost) {
                None => return None,
                Some(cost_diff) => {
                    let (price, amount, cost) = Self::calculate_last_fill_data(
                        last_fill_amount,
                        &order_fills,
                        order_filled_amount,
                        &currency_pair_metadata,
                        cost_diff,
                        &mut event_data,
                    );
                    last_fill_price = price;
                    last_fill_amount = amount;
                    last_fill_cost = cost
                }
            };
        }

        if last_fill_amount.is_zero() {
            warn!(
                "last_fill_amount was received for 0 for {}, {:?}",
                order_ref.client_order_id(),
                order_ref.exchange_order_id()
            );

            return None;
        }

        Some((last_fill_price, last_fill_amount, last_fill_cost))
    }

    fn calculate_cost_diff(
        order_fills: &Vec<OrderFill>,
        order_ref: &OrderRef,
        last_fill_cost: Price,
    ) -> Option<Price> {
        // Diff should be calculated only if it is not the first fill
        let mut total_filled_cost = dec!(0);
        order_fills
            .iter()
            .for_each(|fill| total_filled_cost += fill.cost());
        let cost_diff = last_fill_cost - total_filled_cost;
        if cost_diff <= dec!(0) {
            warn!(
                "cost_diff is {} which is <= 0 for {:?}",
                cost_diff, order_ref
            );

            return None;
        }

        Some(cost_diff)
    }

    fn calculate_last_fill_data(
        last_fill_amount: Amount,
        order_fills: &Vec<OrderFill>,
        order_filled_amount: Amount,
        currency_pair_metadata: &CurrencyPairMetadata,
        cost_diff: Price,
        event_data: &mut FillEventData,
    ) -> (Price, Amount, Price) {
        let amount_diff = last_fill_amount - order_filled_amount;
        let res_fill_price = if !currency_pair_metadata.is_derivative() {
            cost_diff / amount_diff
        } else {
            amount_diff / cost_diff
        };
        let last_fill_price = currency_pair_metadata.price_round(res_fill_price, Round::ToNearest);

        let last_fill_amount = amount_diff;
        let last_fill_cost = cost_diff;

        if let Some(commission_amount) = event_data.commission_amount {
            let mut current_commission = dec!(0);
            order_fills
                .iter()
                .for_each(|fill| current_commission += fill.commission_amount());
            event_data.commission_amount = Some(commission_amount - current_commission);
        }

        (last_fill_price, last_fill_amount, last_fill_cost)
    }

    fn wrong_status_or_cancelled(order_ref: &OrderRef, event_data: &FillEventData) -> Result<()> {
        if order_ref.status() == OrderStatus::FailedToCreate
            || order_ref.status() == OrderStatus::Completed
            || order_ref.was_cancellation_event_raised()
        {
            let error_msg = format!(
                "Fill was received for a {:?} {} {:?}",
                order_ref.status(),
                order_ref.was_cancellation_event_raised(),
                event_data
            );

            error!("{}", error_msg);
            bail!("{}", error_msg)
        }

        Ok(())
    }

    fn get_order_role(event_data: &FillEventData, order_ref: &OrderRef) -> Result<OrderRole> {
        match &event_data.order_role {
            Some(order_role) => Ok(order_role.clone()),
            None => {
                if event_data.commission_amount.is_none()
                    && event_data.commission_rate.is_none()
                    && order_ref.role().is_none()
                {
                    let error_msg = format!("Fill has neither commission nor comission rate",);

                    error!("{}", error_msg);
                    bail!("{}", error_msg)
                }

                match order_ref.role() {
                    Some(role) => Ok(role),
                    None => {
                        let error_msg = format!("Unable to determine order_role");

                        error!("{}", error_msg);
                        bail!("{}", error_msg)
                    }
                }
            }
        }
    }

    fn local_order_exist(
        &self,
        mut event_data: &mut FillEventData,
        order_ref: &OrderRef,
    ) -> Result<()> {
        let (order_fills, order_filled_amount) = order_ref.get_fills();

        if Self::was_trade_already_received(&event_data.trade_id, &order_fills, &order_ref) {
            return Ok(());
        }

        if Self::diff_fill_after_non_diff(&event_data, &order_fills, &order_ref) {
            return Ok(());
        }

        if Self::filled_amount_not_less_event_fill(&event_data, order_filled_amount, &order_ref) {
            return Ok(());
        }

        // FIXME It's not wholly implemented
        let currency_pair_metadata = self.get_currency_pair_metadata(&order_ref.currency_pair())?;
        let last_fill_data = match Self::get_last_fill_data(
            &mut event_data,
            &currency_pair_metadata,
            &order_fills,
            order_filled_amount,
            order_ref,
        ) {
            Some(last_fill_data) => last_fill_data,
            None => return Ok(()),
        };
        let (last_fill_price, last_fill_amount, last_fill_cost) = last_fill_data;

        if let Some(total_filled_amount) = event_data.total_filled_amount {
            if order_filled_amount + last_fill_amount != total_filled_amount {
                warn!(
                    "Fill was missed because {} != {} for {:?}",
                    order_filled_amount, total_filled_amount, order_ref
                );

                return Ok(());
            }
        }

        Self::wrong_status_or_cancelled(&*order_ref, &event_data)?;

        info!("Received fill {:?}", event_data);

        let commission_currency_code = match &event_data.commission_currency_code {
            Some(commission_currency_code) => commission_currency_code.clone(),
            None => currency_pair_metadata.get_commision_currency_code(order_ref.side()),
        };

        let order_role = Self::get_order_role(event_data, order_ref)?;

        // FIXME What is the better name?
        let some_magical_number = dec!(0.01);
        let expected_commission_rate =
            self.commission.get_commission(Some(order_role))?.fee * some_magical_number;

        if event_data.commission_amount.is_none() && event_data.commission_rate.is_none() {
            event_data.commission_rate = Some(expected_commission_rate);
        }

        if event_data.commission_amount.is_none() {
            let last_fill_amount_in_currency_code = currency_pair_metadata
                .convert_amount_from_amount_currency_code(
                    commission_currency_code.clone(),
                    last_fill_amount,
                    last_fill_price,
                );
            event_data.commission_amount = Some(
                last_fill_amount_in_currency_code
                    * event_data.commission_rate.expect(
                        // FIXME that is not true! commission rate can be null here
                        "Impossible sitation: event_data.commission_rate are set above already",
                    ),
            );
        }

        // FIXME refactoring this handling Option<comission_amount>>
        let commission_amount = event_data
            .commission_amount
            .clone()
            .expect("Impossible sitation: event_data.commission_amount are set above already");

        let mut converted_commission_currency_code = commission_currency_code.clone();
        let mut converted_commission_amount = commission_amount;

        if commission_currency_code != currency_pair_metadata.base_currency_code
            && commission_currency_code != currency_pair_metadata.quote_currency_code
        {
            let mut currency_pair = CurrencyPair::from_currency_codes(
                commission_currency_code.clone(),
                currency_pair_metadata.quote_currency_code.clone(),
            );
            match self.top_prices.get(&currency_pair) {
                Some(top_prices) => {
                    let (_, bid) = *top_prices;
                    let price_bnb_quote = bid.0;
                    converted_commission_amount = commission_amount * price_bnb_quote;
                    converted_commission_currency_code =
                        currency_pair_metadata.quote_currency_code.clone();
                }
                None => {
                    currency_pair = CurrencyPair::from_currency_codes(
                        currency_pair_metadata.quote_currency_code.clone(),
                        commission_currency_code,
                    );

                    match self.top_prices.get(&currency_pair) {
                        Some(top_prices) => {
                            let (ask, _) = *top_prices;
                            let price_quote_bnb = ask.0;
                            converted_commission_amount = commission_amount / price_quote_bnb;
                            converted_commission_currency_code =
                                currency_pair_metadata.quote_currency_code.clone();
                        }
                        None => error!(
                            "Top bids and asks for {} and currency pair {:?} do not exist",
                            self.exchange_account_id, currency_pair
                        ),
                    }
                }
            }
        }

        let last_fill_amount_in_converted_commission_currency_code = currency_pair_metadata
            .convert_amount_from_amount_currency_code(
                converted_commission_currency_code,
                last_fill_amount,
                last_fill_price,
            );
        let expected_converted_commission_amount =
            last_fill_amount_in_converted_commission_currency_code * expected_commission_rate;

        let referral_reward_amount = commission_amount
            * self
                .commission
                .get_commission(Some(order_role))?
                .referral_reward
            * some_magical_number;

        let rounded_fill_price =
            currency_pair_metadata.price_round(last_fill_price, Round::ToNearest);
        let order_fill = OrderFill::new(
            // FIXME what to do with it? Does it even use in C#?
            Uuid::new_v4(),
            Utc::now(),
            OrderFillType::Liquidation,
            Some(event_data.trade_id.clone()),
            rounded_fill_price,
            last_fill_amount,
            last_fill_cost,
            order_role.into(),
            CurrencyCode::new("test".into()),
            commission_amount,
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        // FIXME Why should we clone it here?
        order_ref.fn_mut(|order| order.add_fill(order_fill.clone()));
        // This order fields updated, so let's use actual values
        let (order_fills, order_filled_amount) = order_ref.get_fills();

        let mut order_fills_cost_sum = dec!(0);
        order_fills
            .iter()
            .for_each(|fill| order_fills_cost_sum += fill.cost());
        let average_fill_price = if !currency_pair_metadata.is_derivative() {
            order_fills_cost_sum / order_filled_amount
        } else {
            order_filled_amount / order_fills_cost_sum
        };

        order_ref.fn_mut(|order| {
            order.internal_props.average_fill_price =
                currency_pair_metadata.price_round(average_fill_price, Round::ToNearest)
        });

        if order_filled_amount > order_ref.amount() {
            let error_msg = format!(
                "filled_amount {} > order.amount {} for {} {} {:?}",
                order_filled_amount,
                order_ref.amount(),
                self.exchange_account_id,
                order_ref.client_order_id(),
                order_ref.exchange_order_id(),
            );

            error!("{}", error_msg);
            bail!("{}", error_msg)
        }

        if order_filled_amount == order_ref.amount() {
            order_ref.fn_mut(|order| {
                order.set_status(OrderStatus::Completed, Utc::now());
                self.add_event_on_order_change(order, OrderEventType::OrderFilled)
                    .expect("Unable to send event, probably receiver is dead already");
            });
        }

        info!(
            "Added a fill {} {} {} {:?} {:?}",
            self.exchange_account_id,
            event_data.trade_id,
            order_ref.client_order_id(),
            order_ref.exchange_order_id(),
            order_fill
        );

        if event_data.source_type == EventSourceType::RestFallback {
            // TODO some metrics
        }

        if order_ref.status() == OrderStatus::Completed {
            order_ref.fn_mut(|order| {
                order.set_status(OrderStatus::Completed, Utc::now());
                self.add_event_on_order_change(order, OrderEventType::OrderCompleted)
                    .expect("Unable to send event, probably receiver is dead already");
            });
        }

        // TODO DataRecorder.save(order)

        // FIXME handle it in the end
        Ok(())
    }

    fn check_based_on_fill_type(
        &self,
        event_data: &mut FillEventData,
        args_to_log: &ArgsToLog,
    ) -> Result<()> {
        if event_data.fill_type == OrderFillType::Liquidation
            || event_data.fill_type == OrderFillType::ClosePosition
        {
            if event_data.fill_type == OrderFillType::Liquidation
                && event_data.trade_currency_pair.is_none()
            {
                Self::log_fill_handling_error_and_propagate(
                    "Currency pair should be set for liquidation trade",
                    &args_to_log,
                )?;
            }

            if event_data.order_side.is_none() {
                Self::log_fill_handling_error_and_propagate(
                    "Side should be set for liquidatioin or close position trade",
                    &args_to_log,
                )?;
            }

            if event_data.client_order_id.is_some() {
                Self::log_fill_handling_error_and_propagate(
                    "Client order id cannot be set for liquidation or close position trade",
                    &args_to_log,
                )?;
            }

            if event_data.order_amount.is_none() {
                Self::log_fill_handling_error_and_propagate(
                    "Order amount should be set for liquidation or close position trade",
                    &args_to_log,
                )?;
            }

            match self
                .orders
                .by_exchange_id
                .get(&event_data.exchange_order_id)
            {
                Some(order) => {
                    event_data.client_order_id = Some(order.client_order_id());
                }
                None => {
                    // Liquidation and ClosePosition are always Takers
                    let order_instance = self.create_order_instance(event_data, OrderRole::Taker);

                    event_data.client_order_id =
                        Some(order_instance.header.client_order_id.clone());
                    self.handle_create_order_succeeded(
                        &self.exchange_account_id,
                        &order_instance.header.client_order_id,
                        &event_data.exchange_order_id,
                        &event_data.source_type,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn create_order_instance(
        &self,
        event_data: &FillEventData,
        order_role: OrderRole,
    ) -> OrderSnapshot {
        let currency_pair = event_data
            .trade_currency_pair
            .clone()
            .expect("Impossible situation: currency pair are checked above already");
        let order_amount = event_data
            .order_amount
            .clone()
            .expect("Impossible situation: amount are checked above already");
        let order_side = event_data
            .order_side
            .clone()
            .expect("Impossible situation: order_side are checked above already");

        let client_order_id = ClientOrderId::unique_id();

        let order_instance = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(order_role),
            self.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );

        self.orders
            .add_snapshot_initial(Arc::new(RwLock::new(order_instance.clone())));

        order_instance
    }

    fn log_fill_handling_error_and_propagate(
        template: &str,
        args_to_log: &(
            ExchangeAccountId,
            String,
            Option<ClientOrderId>,
            ExchangeOrderId,
            AllowedEventSourceType,
            EventSourceType,
        ),
    ) -> Result<()> {
        let error_msg = format!("{} {:?}", template, args_to_log);

        error!("{}", error_msg);
        bail!("{}", error_msg)
    }

    fn should_ignore_event(
        allowed_event_source_type: AllowedEventSourceType,
        source_type: EventSourceType,
    ) -> bool {
        if allowed_event_source_type == AllowedEventSourceType::FallbackOnly
            && source_type != EventSourceType::RestFallback
        {
            return true;
        }

        if allowed_event_source_type == AllowedEventSourceType::NonFallback
            && source_type != EventSourceType::Rest
            && source_type != EventSourceType::WebSocket
        {
            return true;
        }

        return false;
    }
}

#[cfg(test)]
mod test {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::core::{
        exchanges::binance::binance::Binance, exchanges::common::CurrencyCode,
        exchanges::events::OrderEvent, exchanges::general::commission::Commission,
        exchanges::general::currency_pair_metadata::PrecisionType,
        exchanges::general::features::ExchangeFeatures,
        exchanges::general::features::OpenOrdersType, orders::fill::OrderFill,
        orders::order::OrderExecutionType, orders::order::OrderFillRole, orders::order::OrderFills,
        orders::order::OrderHeader, orders::order::OrderSimpleProps,
        orders::order::OrderStatusHistory, orders::order::SystemInternalOrderProps,
        orders::pool::OrdersPool, settings,
    };
    use std::sync::mpsc::{channel, Receiver};

    fn get_test_exchange() -> (Arc<Exchange>, Receiver<OrderEvent>) {
        let exchange_account_id = ExchangeAccountId::new("local_exchange_account_id".into(), 0);
        let settings = settings::ExchangeSettings::new(
            exchange_account_id.clone(),
            "test_api_key".into(),
            "test_secret_key".into(),
            false,
        );

        let binance = Binance::new(settings, "Binance0".parse().expect("in test"));

        let (tx, rx) = channel();
        let exchange = Exchange::new(
            exchange_account_id,
            "host".into(),
            vec![],
            vec![],
            Box::new(binance),
            ExchangeFeatures::new(
                OpenOrdersType::AllCurrencyPair,
                false,
                true,
                AllowedEventSourceType::default(),
            ),
            tx,
            Commission::default(),
        );

        (exchange, rx)
    }

    mod liquidation {
        use super::*;

        #[test]
        fn empty_currency_pair() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: String::new(),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: None,
                order_side: None,
                order_amount: None,
            };

            let (exchange, _) = get_test_exchange();
            match exchange.handle_order_filled(event_data) {
                Ok(_) => assert!(false),
                Err(error) => {
                    assert_eq!(
                        "Currency pair should be set for liquidation trade",
                        &error.to_string()[..49]
                    );
                }
            }
        }

        #[test]
        fn empty_order_side() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: String::new(),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_currency_codes(
                    "te".into(),
                    "st".into(),
                )),
                order_side: None,
                order_amount: None,
            };

            let (exchange, _) = get_test_exchange();
            match exchange.handle_order_filled(event_data) {
                Ok(_) => assert!(false),
                Err(error) => {
                    assert_eq!(
                        "Side should be set for liquidatioin or close position trade",
                        &error.to_string()[..59]
                    );
                }
            }
        }

        #[test]
        fn not_empty_client_order_id() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: String::new(),
                client_order_id: Some(ClientOrderId::unique_id()),
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_currency_codes(
                    "te".into(),
                    "st".into(),
                )),
                order_side: Some(OrderSide::Buy),
                order_amount: None,
            };

            let (exchange, _) = get_test_exchange();
            match exchange.handle_order_filled(event_data) {
                Ok(_) => assert!(false),
                Err(error) => {
                    assert_eq!(
                        "Client order id cannot be set for liquidation or close position trade",
                        &error.to_string()[..69]
                    );
                }
            }
        }

        #[test]
        fn not_empty_order_amount() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: String::new(),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_currency_codes(
                    "te".into(),
                    "st".into(),
                )),
                order_side: Some(OrderSide::Buy),
                order_amount: None,
            };

            let (exchange, _) = get_test_exchange();
            match exchange.handle_order_filled(event_data) {
                Ok(_) => assert!(false),
                Err(error) => {
                    assert_eq!(
                        "Order amount should be set for liquidation or close position trade",
                        &error.to_string()[..66]
                    );
                }
            }
        }

        #[test]
        fn should_add_order() {
            let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
            let order_side = OrderSide::Buy;
            let order_amount = dec!(12);
            let order_role = None;
            let fill_price = dec!(0.2);
            let fill_amount = dec!(5);

            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: String::new(),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price,
                fill_amount,
                is_diff: false,
                total_filled_amount: None,
                order_role,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(currency_pair.clone()),
                order_side: Some(order_side),
                order_amount: Some(order_amount),
            };

            let (exchange, _event_received) = get_test_exchange();
            match exchange.handle_order_filled(event_data) {
                Ok(_) => {
                    let order = exchange
                        .orders
                        .by_client_id
                        .iter()
                        .next()
                        .expect("order should be added already");
                    assert_eq!(order.order_type(), OrderType::Liquidation);
                    assert_eq!(order.exchange_account_id(), exchange.exchange_account_id);
                    assert_eq!(order.currency_pair(), currency_pair);
                    assert_eq!(order.side(), order_side);
                    assert_eq!(order.amount(), order_amount);
                    assert_eq!(order.price(), fill_price);
                    assert_eq!(order.role(), Some(OrderRole::Taker));

                    let (fills, filled_amount) = order.get_fills();
                    assert_eq!(filled_amount, fill_amount);
                    assert_eq!(fills.iter().next().expect("in test").price(), fill_price);
                }
                Err(_) => assert!(false),
            }
        }

        #[test]
        fn empty_exchange_order_id() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: String::new(),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_currency_codes(
                    "te".into(),
                    "st".into(),
                )),
                order_side: Some(OrderSide::Buy),
                order_amount: Some(dec!(0)),
            };

            let (exchange, _event_receiver) = get_test_exchange();
            match exchange.handle_order_filled(event_data) {
                Ok(_) => assert!(false),
                Err(error) => {
                    assert_eq!(
                        "Received HandleOrderFilled with an empty exchangeOrderId",
                        &error.to_string()[..56]
                    );
                }
            }
        }
    }

    #[test]
    fn ignore_if_trade_was_already_received() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();
        let fill_amount = dec!(0.2);

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(CurrencyPair::from_currency_codes("te".into(), "st".into())),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            Utc::now(),
            OrderFillType::Liquidation,
            Some(trade_id),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        exchange
            .local_order_exist(&mut event_data, &*order_ref)
            .expect("in test");

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, fill_amount);
    }

    #[test]
    fn ignore_diff_fill_after_non_diff() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let fill_amount = dec!(0.2);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(CurrencyPair::from_currency_codes("te".into(), "st".into())),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            Utc::now(),
            OrderFillType::Liquidation,
            Some("different_trade_id".to_owned()),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        exchange
            .local_order_exist(&mut event_data, &*order_ref)
            .expect("in test");

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, fill_amount);
    }

    #[test]
    fn ignore_filled_amount_not_less_event_fill() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let fill_amount = dec!(0.2);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(CurrencyPair::from_currency_codes("te".into(), "st".into())),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            Utc::now(),
            OrderFillType::Liquidation,
            Some("different_trade_id".to_owned()),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        exchange
            .local_order_exist(&mut event_data, &*order_ref)
            .expect("in test");

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, fill_amount);
    }

    #[test]
    fn ignore_diff_fill_if_filled_amount_is_zero() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let fill_amount = dec!(0);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            Utc::now(),
            OrderFillType::Liquidation,
            Some("different_trade_id".to_owned()),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            true,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        exchange
            .local_order_exist(&mut event_data, &*order_ref)
            .expect("in test");

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, dec!(0));
    }

    #[test]
    fn error_if_order_status_is_failed_to_create() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(1);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );
        order.set_status(OrderStatus::FailedToCreate, Utc::now());

        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        match exchange.local_order_exist(&mut event_data, &*order_ref) {
            Ok(_) => assert!(false),
            Err(error) => {
                assert_eq!(
                    "Fill was received for a FailedToCreate false",
                    &error.to_string()[..44]
                );
            }
        }
    }

    #[test]
    fn error_if_order_status_is_completed() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(1);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );
        order.set_status(OrderStatus::Completed, Utc::now());

        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        match exchange.local_order_exist(&mut event_data, &*order_ref) {
            Ok(_) => assert!(false),
            Err(error) => {
                assert_eq!(
                    "Fill was received for a Completed false",
                    &error.to_string()[..39]
                );
            }
        }
    }

    #[test]
    fn error_if_cancellation_event_was_raised() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(1);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();
        let fill_price = dec!(0.2);

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price,
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );
        order.internal_props.cancellation_event_was_raised = true;

        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        match exchange.local_order_exist(&mut event_data, &*order_ref) {
            Ok(_) => assert!(false),
            Err(error) => {
                // TODO has to be Created!
                // Does it mean order status had to be changed somewhere?
                assert_eq!(
                    "Fill was received for a Creating true",
                    &error.to_string()[..37]
                );
            }
        }
    }

    // TODO Can be improved via testing onle calculate_cost_diff_function
    #[test]
    fn calculate_cost_diff() {
        let (exchange, _event_receiver) = get_test_exchange();

        let currency_pair = CurrencyPair::from_currency_codes("phb".into(), "btc".into());
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = "test_trade_id".to_owned();
        let client_order_id = ClientOrderId::unique_id();
        let order_side = OrderSide::Buy;
        let order_price = dec!(0.2);
        let order_role = OrderRole::Maker;
        let exchange_order_id: ExchangeOrderId = "some_order_id".into();

        // Add order manually for setting custom order.amount
        // FIXME ADD order with exchange_order_id
        let header = OrderHeader::new(
            client_order_id.clone(),
            Utc::now(),
            exchange.exchange_account_id.clone(),
            currency_pair.clone(),
            OrderType::Limit,
            OrderSide::Buy,
            order_amount,
            OrderExecutionType::None,
            None,
            None,
            None,
        );
        let props = OrderSimpleProps::new(
            Some(order_price),
            Some(order_role),
            Some(exchange_order_id.clone()),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        let order = OrderSnapshot::new(
            Arc::new(header),
            props,
            OrderFills::default(),
            OrderStatusHistory::default(),
            SystemInternalOrderProps::default(),
        );

        exchange
            .orders
            .try_add_snapshot_by_exchange_id(Arc::new(RwLock::new(order)));
        let base_currency = "PHB";
        let quote_currency = "PHB";
        let specific_currency_pair = "PHBBTC";
        // FIXME What is proper value?
        let price_precision = 0;
        // FIXME What is proper value?
        let amount_precision = 0;
        let price_tick = dec!(0.1);
        let symbol = CurrencyPairMetadata::new(
            false,
            false,
            base_currency.into(),
            base_currency.into(),
            quote_currency.into(),
            quote_currency.into(),
            specific_currency_pair.into(),
            None,
            None,
            price_precision,
            PrecisionType::ByFraction,
            Some(price_tick),
            base_currency.into(),
            None,
            None,
            amount_precision,
            PrecisionType::ByFraction,
            None,
            None,
            None,
        );
        exchange.symbols.lock().push(Arc::new(symbol));

        let first_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.01)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
        };

        exchange
            .handle_order_filled(first_event_data)
            .expect("in test");

        let second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: "another_trade_id".to_owned(),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.3),
            fill_amount: dec!(10),
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.03)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        exchange
            .handle_order_filled(second_event_data)
            .expect("in test");

        let order_ref = exchange
            .orders
            .by_exchange_id
            .get(&exchange_order_id)
            .expect("in test");
        let (fills, _filled_amount) = order_ref.get_fills();

        assert_eq!(fills.len(), 2);
        let first_fill = &fills[0];
        assert_eq!(first_fill.price(), dec!(0.2));
        assert_eq!(first_fill.amount(), dec!(5));
        assert_eq!(first_fill.commission_amount(), dec!(0.01));
        let second_fill = &fills[1];
        assert_eq!(second_fill.price(), dec!(0.4));
        assert_eq!(second_fill.amount(), dec!(5));
        assert_eq!(second_fill.commission_amount(), dec!(0.02));
    }

    #[test]
    fn ignore_fill_if_total_filled_amount_is_incorrect() {
        let (exchange, _event_receiver) = get_test_exchange();

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_currency_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(1);
        let trade_id = "test_trade_id".to_owned();

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: trade_id.clone(),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: Some(dec!(9)),
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair.clone()),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id.clone(),
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        let order_ref = order_pool
            .by_client_id
            .get(&client_order_id)
            .expect("in test");

        match exchange.local_order_exist(&mut event_data, &*order_ref) {
            Ok(_) => {
                let (fills, _) = order_ref.get_fills();
                assert!(fills.is_empty());
            }
            Err(_) => assert!(false),
        }
    }
}
