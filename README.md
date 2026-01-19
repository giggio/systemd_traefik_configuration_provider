# Traefik Configuration Provider from systemd

![Build status](https://codeberg.org/giggio/systemd_traefik_configuration_provider/badges/workflows/build.yaml/badge.svg)

Main repo: [codeberg.org/giggio/systemd_traefik_configuration_provider](https://codeberg.org/giggio/systemd_traefik_configuration_provider)

This app will gather info from systemd and publish it as YAML configuration in the
[Traefik format](https://doc.traefik.io/traefik/reference/routing-configuration/other-providers/file/).

## Quick Start

Create a systemd service annotated with Traefik configuration. This is using the
same style as the
[Docker configuration](https://doc.traefik.io/traefik/reference/install-configuration/providers/docker/).

You can use the example sleep service at [./test/sleep.service](./test/sleep.service).
Clone this repo, `cd` to it, then:

```bash
# copy the service
sudo cp test/sleep.service /etc/systemd/system/sleep.service
# reload the systemd daemon
sudo systemctl daemon-reload
# test the service
sudo systemctl start sleep
sudo systemctl status sleep
```

Run the application with cargo:

```bash
TRAEFIK_OUT_DIR=`pwd`/test/units/ RUST_LOG=systemd_traefik_configuration_provider=trace cargo run
```

Files will be generated at the `TRAEFIK_OUT_DIR` environment variable location. If not set, they will output to:
`/etc/traefik/dynamic/units`.

The application does not take any command line argument.

### Logging

Logging is controlled by environment variable `RUST_LOG`, as is common with
Rust applications. It can be set to error, warn, info, debug or trace. Setting
it will enable logs for all libraries used by the application, so it might be
better to prefix it with `systemd_traefik_configuration_provider=`, e.g.
`RUST_LOG=systemd_traefik_configuration_provider=info`, to only get logs from
this application. Setting it without prefix will show logs from other libraries.
The default is
no logs.
Logs are output to stdout.

## Releasing

Releases are being created using Make and Nix cross compilation. See [./cross-build.nix](./cross-build.nix) and
[./flake.nix](./flake.nix) and [./Makefile](./Makefile).

To release, simply run `make`, which will build static binaries for amd64 and arm64 using musl.

## Testing

Run `cargo nextest run --no-fail-fast` to get the test report.

To run an end to end test, run:

```bash
while true; do sudo systemctl stop sleep.service; sleep 0.1; ! [ -f test/units/sleep.service.yml ] || break; sudo systemctl start sleep.service; sleep 0.1; [ -f test/units/sleep.service.yml ] || break; echo -n .; done
```

This will run until it fails or you press ctrl-c.

## Contributing

Questions, comments, bug reports, and pull requests are all welcome.  Submit them at
[the project on Codeberg](https://codeberg.org/giggio/systemd_traefik_configuration_provider/).

Bug reports that include steps-to-reproduce (including code) are the best. Even better, make them in the form of pull
requests. Pull requests on Github will probably be ignored, so avoid them.

## Author

[Giovanni Bassi](https://links.giggio.net/bio)

## License

Licensed under the MIT license.
