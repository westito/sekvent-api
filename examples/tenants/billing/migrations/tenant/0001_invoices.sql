CREATE TABLE invoice_series (
    name text PRIMARY KEY,
    prefix text NOT NULL,
    currency text NOT NULL,
    next_number bigint NOT NULL DEFAULT 1
);

CREATE TABLE invoices (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    number text NOT NULL UNIQUE,
    customer text NOT NULL,
    amount_cents bigint NOT NULL CHECK (amount_cents >= 0),
    currency text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE owner_invites (
    token_sha256 text PRIMARY KEY,
    email text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
