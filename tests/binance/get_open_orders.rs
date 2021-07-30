use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use log::warn;
use mmb_lib::core::exchanges::general::commission::Commission;
use mmb_lib::core::exchanges::general::exchange::*;
use mmb_lib::core::exchanges::general::exchange_creation;
use mmb_lib::core::exchanges::general::features::*;
use mmb_lib::core::exchanges::{binance::binance::*, events::AllowedEventSourceType};
use mmb_lib::core::exchanges::{common::*, timeouts::timeout_manager::TimeoutManager};
use mmb_lib::core::lifecycle::cancellation_token::CancellationToken;
use mmb_lib::core::logger::init_logger;
use mmb_lib::core::orders::order::*;
use mmb_lib::core::settings;
use mmb_lib::core::settings::CurrencyPairSetting;
use rust_decimal_macros::*;
use smallstr::SmallString;

use crate::get_binance_credentials_or_exit;
use mmb_lib::core::exchanges::traits::ExchangeClientBuilder;
use mmb_lib::core::lifecycle::application_manager::ApplicationManager;
use tokio::sync::broadcast;

#[actix_rt::test]
async fn open_orders_exists() {
    let (api_key, secret_key) = get_binance_credentials_or_exit!();

    let exchange_account_id: ExchangeAccountId = "Binance0".parse().expect("in test");

    let mut settings = settings::ExchangeSettings::new_short(
        exchange_account_id.clone(),
        api_key,
        secret_key,
        false,
    );

    let application_manager = ApplicationManager::new(CancellationToken::new());
    let (tx, _rx) = broadcast::channel(10);

    BinanceBuilder.extend_settings(&mut settings);
    settings.websocket_channels = vec!["depth".into(), "trade".into()];

    let binance = Binance::new(
        exchange_account_id.clone(),
        settings,
        tx.clone(),
        application_manager.clone(),
    );

    let exchange = Exchange::new(
        exchange_account_id.clone(),
        Box::new(binance),
        ExchangeFeatures::new(
            OpenOrdersType::AllCurrencyPair,
            false,
            true,
            AllowedEventSourceType::default(),
            AllowedEventSourceType::default(),
        ),
        tx,
        application_manager,
        TimeoutManager::new(HashMap::new()),
        Commission::default(),
    );

    exchange.clone().connect().await;

    let test_order_client_id = ClientOrderId::unique_id();
    let test_currency_pair = CurrencyPair::from_codes("phb".into(), "btc".into());
    let test_price = dec!(0.00000005);
    let order_header = OrderHeader::new(
        test_order_client_id.clone(),
        Utc::now(),
        exchange_account_id.clone(),
        test_currency_pair.clone(),
        OrderType::Limit,
        OrderSide::Buy,
        dec!(10000),
        OrderExecutionType::None,
        None,
        None,
        "FromGetOpenOrdersTest".to_owned(),
    );

    let order_to_create = OrderCreating {
        header: order_header,
        price: test_price,
    };

    // Should be called before any other api calls!
    exchange.build_metadata().await;
    let _ = exchange
        .cancel_all_orders(test_currency_pair.clone())
        .await
        .expect("in test");

    let created_order_fut = exchange.create_order(&order_to_create, CancellationToken::default());

    const TIMEOUT: Duration = Duration::from_secs(5);
    let created_order = tokio::select! {
        created_order = created_order_fut => created_order,
        _ = tokio::time::sleep(TIMEOUT) => panic!("Timeout {} secs is exceeded", TIMEOUT.as_secs())
    };

    if let Err(error) = created_order {
        dbg!(&error);
        assert!(false)
    }

    let second_test_order_client_id = ClientOrderId::unique_id();
    let second_order_header = OrderHeader::new(
        second_test_order_client_id.clone(),
        Utc::now(),
        exchange_account_id.clone(),
        test_currency_pair.clone(),
        OrderType::Limit,
        OrderSide::Buy,
        dec!(10000),
        OrderExecutionType::None,
        None,
        None,
        "FromGetOpenOrdersTest".to_owned(),
    );

    let second_order_to_create = OrderCreating {
        header: second_order_header,
        price: test_price,
    };
    log::warn!("hello");
    let _ = exchange.get_open_orders().await.expect("in test");
    log::warn!("hello1");
    assert!(false);

    let created_order_fut =
        exchange.create_order(&second_order_to_create, CancellationToken::default());

    let created_order = tokio::select! {
        created_order = created_order_fut => created_order,
        _ = tokio::time::sleep(TIMEOUT) => panic!("Timeout {} secs is exceeded", TIMEOUT.as_secs())
    };

    match created_order {
        Ok(_order_ref) => {
            let all_orders = exchange.get_open_orders().await.expect("in test");
            assert!(!all_orders.is_empty())
        }

        // Create order failed
        Err(error) => {
            dbg!(&error);
            assert!(false)
        }
    }
}

#[actix_rt::test]
async fn open_orders_by_currency_pair_exists() {
    let (api_key, secret_key) = get_binance_credentials_or_exit!();

    init_logger();
    log::warn!("hello world1");

    let exchange_account_id: ExchangeAccountId = "Binance0".parse().expect("in test");

    let mut settings = settings::ExchangeSettings::new_short(
        exchange_account_id.clone(),
        api_key,
        secret_key,
        false,
    );

    let application_manager = ApplicationManager::new(CancellationToken::new());
    let (tx, _rx) = broadcast::channel(10);

    BinanceBuilder.extend_settings(&mut settings);
    settings.websocket_channels = vec!["depth".into(), "trade".into()];
    let currency_pair_setting = CurrencyPairSetting {
        base: CurrencyCode::new(SmallString::from("phb")),
        quote: CurrencyCode::new(SmallString::from("btc")),
        currency_pair: None,
    };
    settings.currency_pairs = Some(vec![currency_pair_setting]);
    let binance = Binance::new(
        exchange_account_id.clone(),
        settings.clone(),
        tx.clone(),
        application_manager.clone(),
    );
    let exchange = Exchange::new(
        exchange_account_id.clone(),
        Box::new(binance),
        ExchangeFeatures::new(
            OpenOrdersType::OneCurrencyPair,
            false,
            true,
            AllowedEventSourceType::default(),
            AllowedEventSourceType::default(),
        ),
        tx,
        application_manager,
        TimeoutManager::new(HashMap::new()),
        Commission::default(),
    );

    exchange.clone().connect().await;

    let test_order_client_id = ClientOrderId::unique_id();
    let test_currency_pair = CurrencyPair::from_codes("phb".into(), "btc".into());
    let second_test_currency_pair = CurrencyPair::from_codes("troy".into(), "btc".into());

    let order_header = OrderHeader::new(
        test_order_client_id.clone(),
        Utc::now(),
        exchange_account_id.clone(),
        test_currency_pair.clone(),
        OrderType::Limit,
        OrderSide::Buy,
        dec!(2000),
        OrderExecutionType::None,
        None,
        None,
        "FromGetOpenOrdersTest".to_owned(),
    );

    let order_to_create = OrderCreating {
        header: order_header,
        price: dec!(0.00000005),
    };

    // Should be called before any other api calls!
    exchange.build_metadata().await;
    if let Some(currency_pairs) = &settings.currency_pairs {
        exchange.set_symbols(exchange_creation::get_symbols(
            &exchange,
            &currency_pairs[..],
        ))
    }
    let _ = exchange
        .cancel_all_orders(test_currency_pair.clone())
        .await
        .expect("in test");

    let _ = exchange
        .cancel_all_orders(second_test_currency_pair.clone())
        .await
        .expect("in test");

    let created_order_fut = exchange.create_order(&order_to_create, CancellationToken::default());

    const TIMEOUT: Duration = Duration::from_secs(5);
    let created_order = tokio::select! {
        created_order = created_order_fut => created_order,
        _ = tokio::time::sleep(TIMEOUT) => panic!("Timeout {} secs is exceeded", TIMEOUT.as_secs())
    };

    if let Err(error) = created_order {
        dbg!(&error);
        assert!(false)
    }

    let second_test_order_client_id = ClientOrderId::unique_id();
    let second_order_header = OrderHeader::new(
        second_test_order_client_id.clone(),
        Utc::now(),
        exchange_account_id.clone(),
        second_test_currency_pair.clone(),
        OrderType::Limit,
        OrderSide::Buy,
        dec!(2000),
        OrderExecutionType::None,
        None,
        None,
        "FromGetOpenOrdersTest".to_owned(),
    );
    let second_order_to_create = OrderCreating {
        header: second_order_header,
        price: dec!(0.00000005),
    };

    let created_order_fut =
        exchange.create_order(&second_order_to_create, CancellationToken::default());

    let created_order = tokio::select! {
        created_order = created_order_fut => created_order,
        _ = tokio::time::sleep(TIMEOUT) => panic!("Timeout {} secs is exceeded", TIMEOUT.as_secs())
    };

    if let Err(error) = created_order {
        dbg!(&error);
        assert!(false)
    }

    log::warn!("hello world2");
    let all_orders = exchange.get_open_orders().await.expect("in test");
    for order in &all_orders {
        warn!("order currency pair {}", order.currency_pair);
    }

    let _ = exchange
        .cancel_all_orders(test_currency_pair.clone())
        .await
        .expect("in test");

    let _ = exchange
        .cancel_all_orders(second_test_currency_pair.clone())
        .await
        .expect("in test");

    assert_eq!(all_orders.len(), 1);
}
