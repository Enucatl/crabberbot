``
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') cargo build
```

```
CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --profile test run --build --rm test-runner

CARGO_PACKAGE_VERSION=$(git describe --long | sed 's/-/\./') docker compose --env-file .env up --build
```
