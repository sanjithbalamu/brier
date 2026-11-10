-- Durable cancellation deadlines survive gateway restarts. Expired rows remain armed so
-- new orders cannot bypass a missed heartbeat before the maker explicitly rearms the lease.
CREATE TABLE order_leases (
    user_id bigint PRIMARY KEY REFERENCES users(id),
    expires_at bigint NOT NULL,
    cancelled boolean NOT NULL DEFAULT false
);
CREATE INDEX order_leases_due ON order_leases(expires_at) WHERE NOT cancelled;
