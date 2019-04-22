juju-bundle
===========

[![Snap Status](https://build.snapcraft.io/badge/knkski/juju-bundle.svg)](https://build.snapcraft.io/user/knkski/juju-bundle)

This repository hosts a Juju plugin that makes it easy to interact with a Juju
bundle.


Setup
-----

You can install this plugin with `snap`:

    sudo snap install juju-bundle --classic --edge

Or if you want to run this plugin from source, clone this repo with

    https://github.com/knkski/juju-bundle.git
    cd juju-bundle

You will also need to install the Rust compiler. Instructions can be found at
https://rustup.rs/.


Usage
-----

You can run this plugin and just pass in the appropriate juju `deploy`
commands:

    # Installed via snap
    juju bundle deploy bundle.yaml

    # Running from source
    cargo run --bin juju-bundle deploy bundle.yaml

Note that both `cargo` and this plugin can take arguments, so to pass
options or parameters to `juju deploy` itself, you will want to call it like
so:

    # Installed via snap
    juju bundle deploy bundle.yaml -- -m model-name

    # Running from source
    cargo run --bin juju-bundle -- deploy bundle.yaml -- -m model-name
