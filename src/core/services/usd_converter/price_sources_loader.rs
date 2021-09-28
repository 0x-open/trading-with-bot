use std::collections::HashMap;

use crate::core::{
    exchanges::common::TradePlace, lifecycle::cancellation_token::CancellationToken,
    misc::price_by_order_side::PriceByOrderSide, DateTime,
};

pub(crate) struct PriceSourcesLoader {
    // TODO: fix when DatabaseManager will be added
//database_manager: DatabaseManager
}

impl PriceSourcesLoader {
    pub fn new(//database_manager: DatabaseManager
    ) -> Self {
        Self{
            //database_manager: DatabaseManager
        }
    }

    pub async fn load(
        save_time: DateTime,
        cancellation_token: CancellationToken,
    ) -> HashMap<TradePlace, PriceByOrderSide> {
        //     const string sqlQuery =
        //         "SELECT a.* FROM public.\"PriceSources\" a " +
        //         "JOIN ( " +
        //         "SELECT \"ExchangeName\", \"CurrencyCodePair\", max(\"DateTime\") \"DateTime\" " +
        //         "FROM public.\"PriceSources\" " +
        //         "WHERE \"DateTime\" <= {0} " +
        //         "GROUP BY \"ExchangeName\", \"CurrencyCodePair\" " +
        //         ") b ON a.\"ExchangeName\" = b.\"ExchangeName\" AND a.\"CurrencyCodePair\" = b.\"CurrencyCodePair\" AND a.\"DateTime\" = b.\"DateTime\"";

        //     await using var session = _databaseManager.Sql;
        //     return await session.Set<PriceSourceModel>()
        //         .FromSqlRaw(sqlQuery, dateTime)
        //         .ToDictionaryAsync(
        //             x => new ExchangeNameSymbol(x.ExchangeName, x.CurrencyCodePair),
        //             x => new PricesBySide(x.Ask, x.Bid),
        //             cancellationToken);

        HashMap::new()
    }
}
