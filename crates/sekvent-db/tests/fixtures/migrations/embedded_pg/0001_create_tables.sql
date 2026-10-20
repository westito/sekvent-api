CREATE TABLE customers (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE orders (
    id BIGSERIAL PRIMARY KEY,
    customer_id UUID NOT NULL REFERENCES customers (id),
    reference TEXT NOT NULL,
    total_cents BIGINT NOT NULL,
    attributes JSONB NOT NULL DEFAULT '{}',
    tags TEXT[] NOT NULL DEFAULT '{}',
    placed_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT orders_reference_key UNIQUE (reference),
    CONSTRAINT orders_total_non_negative CHECK (total_cents >= 0)
);
