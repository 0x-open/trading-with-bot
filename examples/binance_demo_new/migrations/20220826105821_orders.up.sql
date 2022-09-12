CREATE TABLE orders (
    id bigint PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    insert_time timestamp WITH TIME ZONE NOT NULL DEFAULT now(),
    version int,
    json jsonb NOT NULL
);

CREATE INDEX orders__insert_time_idx ON orders USING btree (insert_time);
CREATE INDEX orders__client_order_id_idx ON orders USING btree (((json #>> '{header, client_order_id}')::text));
CREATE INDEX orders__exchange_account_id_idx ON orders USING btree (((json #>> '{header, exchange_account_id}')::text));
CREATE INDEX orders__finished_time_idx ON orders USING btree (((json #>> '{props, finished_time}')::text));
CREATE INDEX orders__exchange_order_id_idx ON orders USING btree (((json #>> '{props, exchange_order_id}')::text));
