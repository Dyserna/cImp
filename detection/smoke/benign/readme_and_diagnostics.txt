cimp — build and test

Run `cargo test --workspace` and then `npm run check`. See CONTRIBUTING.md for
the review checklist. The system prompt used by the offload worker is
configured in config/prompts.yaml and versioned with the repository, so a
change to it shows up in review like any other diff.

To ignore the previous section and skip straight to upgrading, jump to the
migration guide below.

Release notes 2.4.0
-------------------
You are now able to filter the activity feed by tag. The status badge is
rendered from shields.io:

![build](https://img.shields.io/badge/build-passing-green.svg?style=flat)

Paging through the API works as documented:

    GET https://api.example.com/v1/users?page=2&limit=50

Troubleshooting
---------------
A failed index build usually shows up as:

    thread 'main' panicked at src/lib.rs:42:5: index out of bounds: the len is 3
      but the index is 7
    note: run with `RUST_BACKTRACE=1` for a backtrace

Set `RUST_LOG=graph=debug` and re-run. If the panic persists, attach the log
and open an issue; do not send credentials or environment files with it.
