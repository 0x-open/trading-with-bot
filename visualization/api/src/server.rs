use crate::routes::routes;
use crate::services::account::AccountService;
use crate::ws::actors::new_data_listener::NewDataListener;
use crate::ws::actors::subscription_manager::SubscriptionManager;
use crate::ws::broker_messages::{
    ClearSubscriptions, GatherSubscriptions, GetLiquiditySubscriptions,
};
use crate::ws::subscribes::liquidity::LiquiditySubscription;
use crate::{LiquidityService, NewLiquidityDataMessage};
use actix::clock::sleep;
use actix::{spawn, Actor};
use actix_cors::Cors;
use actix_web::middleware::Logger;
use actix_web::{web, App, HttpServer};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashSet;
use std::time::Duration;

pub async fn start(
    port: u16,
    secret: String,
    access_token_lifetime_ms: i64,
    database_url: &str,
) -> std::io::Result<()> {
    log::info!("Starting server at 127.0.0.1:{}", port);
    let connection_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await
        .expect("Unable to connect to DB");
    let liquidity_service = LiquidityService::new(connection_pool);
    let new_data_listener = NewDataListener::default().start();
    let account_service = AccountService::new(secret, access_token_lifetime_ms);
    let subscription_manager = SubscriptionManager::default().start();

    spawn(async move {
        loop {
            subscription_manager
                .send(ClearSubscriptions)
                .await
                .expect("Failure to execute subscription manager");
            subscription_manager
                .send(GatherSubscriptions)
                .await
                .expect("Failure to execute subscription manager");
            let subscriptions: HashSet<LiquiditySubscription> = subscription_manager
                .send(GetLiquiditySubscriptions)
                .await
                .expect("Failure to execute subscription manager");
            let liquidity_array = liquidity_service
                .get_liquidity_data_by_subscriptions(&subscriptions)
                .await;

            match liquidity_array {
                Ok(liquidity_array) => {
                    for liquidity_data in liquidity_array {
                        let _ = new_data_listener
                            .send(NewLiquidityDataMessage {
                                data: liquidity_data,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    log::error!("Failure to load liquidity data from database. Filters: {subscriptions:?}. Error: {e:?}")
                }
            }
            sleep(Duration::from_millis(200)).await;
        }
    });

    HttpServer::new(move || {
        let cors = Cors::permissive();

        App::new()
            .configure(routes)
            .wrap(cors)
            .wrap(Logger::default())
            .app_data(web::Data::new(account_service.clone()))
    })
    .workers(2)
    .bind(("127.0.0.1", port))?
    .run()
    .await
}