# Example: shop

A small shop built from three sekvent components — inventory, notifications
and orders — running in one binary. It shows milestone C1 of the
[component model](../../docs/component-model.md) (spec:
[component-c1.md](../../docs/design/component-c1.md)): declaring a component,
implementing it, wiring components together by constructor injection, and
moving every call behind a serialization boundary by configuration alone.

## Crates

| Crate | What it holds |
|---|---|
| `inventory-api` | `shop.inventory.v1` messages, the `Inventory` trait (`reserve`, `release`, `stock`), `InventoryError` |
| `inventory` | `InventoryService`: in-memory stock and reservations |
| `notifications-api` | `shop.notifications.v1` messages, the `Notifications` trait (`notify`, `sent`), `NotificationsError` |
| `notifications` | `NotificationsService`: in-memory outbox with a blocklist; opens and closes through `Lifecycle` |
| `orders-api` | `shop.orders.v1` messages, the `Orders` trait (`place_order`, `get_order`), `OrdersError` |
| `orders` | `OrdersService`: in-memory orders; calls inventory and notifications through their handles |
| `shop` | The wiring (`shop::install`), a thin `main.rs`, and the test suite |

An `-api` crate is all a caller needs. `orders` depends on `inventory-api`
and `notifications-api`, never on the implementations.

## How a call flows

`place_order` checks the quantity, reserves stock, notifies the customer and
stores the order. If the notification is refused, it releases the
reservation and returns `OrdersError::CustomerBlocked`. Inventory's
`OutOfStock` and `UnknownSku` are mapped to the orders variants of the same
name; anything else becomes `OrdersError::Other`.

## Bindings

Nothing in the code picks a binding. The App builder reads it:

| Key | Effect |
|---|---|
| (none) | every component `local`: direct calls, no encoding |
| `SEKVENT_COMPONENT_BINDING=local-serialized` | every component behind a prost encode, a separate task and a decode |
| `SEKVENT_COMPONENT_INVENTORY_BINDING=local-serialized` | only inventory |
| `SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT=250ms` | override `reserve`'s `2s` timeout |
| `SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT=4` | override `reserve`'s bulkhead of 16 |

An unknown `SEKVENT_COMPONENT_*` key, a malformed value or `grpc` (not
available in C1) fails the build with a message that names the key.

## Running

```sh
cargo run -p shop                                                   # all local
SEKVENT_COMPONENT_BINDING=local-serialized cargo run -p shop        # all serialized
```

The binary runs the components as one unit of the sekvent runtime until
`SIGINT` or `SIGTERM`.

## Tests

```sh
cargo test -p shop
```

Every test runs twice, once per profile, through `rstest` cases:

- `monolith-local` — no keys, every component `local`;
- `monolith-serialized` — `SEKVENT_COMPONENT_BINDING=local-serialized`.

| File | Covers |
|---|---|
| `tests/flows.rs` | placing and reading orders, out-of-stock, unknown SKU, zero quantity, a blocked customer (reservation released), an unknown order, the caller's tenant reaching notifications |
| `tests/faults.rs` | method, caller and configured deadlines; a full bulkhead; cancelled and expired contexts; typed errors and an unknown reason; dropping the caller; draining on stop |
| `tests/runtime.rs` | the App as a runtime unit: start, one order, shutdown, every component stopped |

Fault tests swap the real inventory for a fake installed through
`InventoryHandle::install`, so the fake sits behind the same binding. Timed
tests use tokio's paused clock; none of them sleeps.
