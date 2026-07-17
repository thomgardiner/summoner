# Example work orders

Annotated work orders for a two-order fleet, written against an imaginary
web service:

- `orders/add-endpoint.toml`: a standalone order with a comment explaining
  every field it sets.
- `orders/document-endpoint.toml`: a dependent order showing the difference
  between `after` (run ordering and failure propagation) and `base` (branch
  chaining, so the dependent worktree starts from the dependency's finished
  work on `grove/smn-add-endpoint`).

The full field reference is in the repository README under "Work-order
fields"; every field is defined in `src/order.rs`.

## Validate

Parse and validate the batch without dispatching anything:

```sh
summoner check examples/orders/
```

This needs a `.summoner.toml` with a configured executor (`summoner init`
creates one). The scopes here point at files of an imaginary service, so
run the orders themselves in a scratch repository, not this one.

## Run

```sh
summoner run examples/orders/
```

Summoner dispatches `add-endpoint` first, waits for it to verify, then runs
`document-endpoint` from the finished branch, and prints one ranked JSON
report.
