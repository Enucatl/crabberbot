```
CARGO_PACKAGE_VERSION=$(git describe --long) cargo build
```

```
CARGO_PACKAGE_VERSION=$(git describe --long) docker compose --profile test run --build --rm test-runner
```
