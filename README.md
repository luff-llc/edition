# Edition

Release lines, tags, channels and a deploy ledger for a small shop that ships a library,
servers and apps from several repositories, much of it written by agents.

Edition answers three questions from files in the repository, not from memory:

- **May this ref go there?** Every channel (a staging server, a TestFlight group, a Play
  track, a production home) has a tier, and the tier decides which refs it accepts.
- **What exactly will happen?** Every mutating command prints the commands it would run.
  Nothing runs without `--apply`, and nothing leaves the machine without `--confirm`.
- **What is deployed where?** Each deploy, pushed tag and forward-merge pull request is
  appended to a ledger in the repository, with the commit, the tree, who ran it, and the
  checks that passed. `edition verify` checks the ledger, the tags, the version files and
  the pinned dependencies against each other and against the remote.

Edition orchestrates the tools a repository already has. It does not build, sign, upload
or hold credentials; it calls the script that does, at the commit it has checked.

**Status: 0.1, in development.** The model and commands below work and are tested; see
[docs/design.md](docs/design.md) for what is deliberately left out and what comes next.

## The model in brief

- `main` carries the next version and builds as `X.Y.0-dev`.
- `release/X.Y` carries a shipped line and takes fixes; lines merge forward into `main`,
  never the reverse.
- Tags are made on release lines: `vX.Y.Z` for releases, `vX.Y.Z-rc.N` for candidates.
- Three tiers, built in and not configurable per channel:

| Tier | Accepts | Approval | Ledger |
| --- | --- | --- | --- |
| `dev` | any ref, on this machine | no | not recorded |
| `staging` | the tip of `main`, nothing else | no | recorded |
| `prod` | a final tag on its release line, present on the remote, with every pinned dependency on a final tag | when the channel asks | recorded |

A prod channel may opt in to release candidates (`allow_prerelease`) for store review
builds; a production server never should.

## Install

```sh
cargo install --path .
```

Edition needs `git` on `PATH`. It uses Concordance's CLI when a repository configures a
catalog, and `gh` only for `edition forward --apply`.

## Use

```sh
edition status                         # lines, tags, what each channel and platform holds
edition plan 0.6.0                     # what the release needs; what Concordance says it carries
edition verify --online                # every invariant; exit 1 on an error

edition deploy staging-home main                          # dry run: prints the plan
edition deploy staging-home main --apply --confirm staging-home

edition cut 0.6 --apply --confirm push                    # release/0.6 from main
edition bump 0.6.0 --apply                                # every version file
edition rc 0.6.0 --apply --confirm push                   # v0.6.0-rc.1, rc.2, ...
edition tag 0.6.0 --apply --confirm push                  # v0.6.0
edition deploy production-home v0.6.0 --approved-by <name> --apply --confirm production-home
edition promote staging-home production-home              # what staging holds, if tagged

edition hotfix start 0.5 --apply                          # hotfix/0.5.3 from release/0.5
edition hotfix finish 0.5.3                               # tag, then offer the forward merge
edition forward --apply --confirm pr                      # pull requests: release/* -> main

edition hooks install --apply                             # pre-push: content, versions, pins
edition ledger --channel staging-home
```

Exit codes: `0` success or a printed dry run, `1` refused or a failed check, `2` usage or
configuration error.

## Configuration

One `edition.toml` at the repository root. A short example:

```toml
product = "colophon"

[[version_files]]
path = "Cargo.toml"
after = "[workspace.package]"
pattern = 'version = "{version}"'

[platforms.home]

[checks.rust]
run = ["cargo", "test", "--workspace", "--locked"]

[channels.staging-home]
tier = "staging"
kind = "home"
target = "https://staging.example.com"
platform = "home"
checks = ["rust"]
run = ["{var.home_kit}/deploy.sh", "release", "{commit}"]
probe = ["curl", "-fsS", "https://staging.example.com/healthz"]

[vars]
home_kit = ""        # set per machine: EDITION_VAR_HOME_KIT
```

Full examples: [examples/colophon](examples/colophon/edition.toml) (a library with
homes) and [examples/marginalia](examples/marginalia/edition.toml) (iOS and Android apps
that pin an SDK and plan releases in Concordance). Every key is described in
[docs/design.md](docs/design.md#configuration).

## Development

```sh
cargo test        # unit tests and scenario tests against temporary git repositories
cargo clippy --all-targets
```

The scenario tests create bare repositories as remotes and never touch the network.
Dependencies are kept to `serde`, `serde_json` and `toml`, at exact versions with default
features off.

## License

Apache-2.0.
