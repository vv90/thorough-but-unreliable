# Order summary

This application reads a JSON configuration with an `input` path, then reads
the order data and prints the order count and total cost.

Run the application:

```sh
orders-summary /work/config.json
```

Run its acceptance check:

```sh
check-orders
```

There is one configuration defect. Inspect the files, repair `/work/config.json`,
and rerun the acceptance check. Keep the application and order data unchanged.
No downloads or dependency installation are needed. Commands start in `/work`.
