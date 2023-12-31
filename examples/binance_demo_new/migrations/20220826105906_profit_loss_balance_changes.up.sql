CREATE TABLE profit_loss_balance_changes (
    id bigint PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    insert_time timestamp WITH TIME ZONE NOT NULL DEFAULT now(),
    version int,
    json jsonb NOT NULL
);

CREATE INDEX profit_loss_balance_changes__insert_time_idx ON profit_loss_balance_changes USING btree (insert_time);
