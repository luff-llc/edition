# Edition

Release lines, tags, channels and a deploy ledger for a small shop that ships a library,
servers and apps from several repositories, much of it written by agents.

Edition answers three questions from files in the repository, not from memory:

- **May this ref go there?** Every channel (a staging server, a TestFlight group, a Play
  track, a production server) has a tier, and the tier decides which refs it accepts.
  The decision is made against the remote's live heads and tags.
- **What exactly will happen?** Every mutating command prints the plan first. A plan
  that leaves the machine runs only with the **receipt** its dry run issued: a token
  derived from a hash of the plan, valid once, for 30 minutes, and only while the refs,
  the configuration and the remote are exactly as they were checked.
- **What is deployed where?** Each deploy, pushed branch or tag, and forward-merge pull
  request is appended to a hash-chained ledger in the repository, with the commit, the
  tree, who ran it, the receipt and the steps that ran. `edition verify` checks the
  ledger, tags, version files and pinned dependencies against each other and the remote.

Edition orchestrates the tools a repository already has. It does not build, sign, upload
or hold credentials; it runs the command that does, as an argument list, at the commit
it has checked.

**Status: 0.1, in development.** What is described here is implemented and tested;
[docs/design.md](docs/design.md) covers the model, what is deliberately left out and what
comes next.

## The model in brief

- `main` carries the next version and builds as `X.Y.0-dev`.
- `release/X.Y` carries a shipped line and takes fixes; lines merge forward into `main`,
  never the reverse.
- Tags are made on release lines: `vX.Y.Z` for releases, `vX.Y.Z-rc.N` for candidates.
- Three tiers, built in and not configurable per channel:

| Tier | Accepts | Approval | Ledger |
| --- | --- | --- | --- |
| `dev` | any ref, on this machine | no | not recorded |
| `staging` | exactly the live tip of `main`, nothing else | no | recorded |
| `prod` | a release tag named explicitly, on the remote and on its release line, with every pinned dependency on a release tag | when the channel asks | recorded |

Release candidates are accepted only by prod channels of a review or testing kind that
set `allow_prerelease` (TestFlight external testing, Play closed testing); a production
server cannot set it.

## Install

```sh
cargo install --path .
```

Edition needs `git` on `PATH`. It uses Concordance's CLI when a repository configures a
catalog, and `gh` for forward-merge pull requests.

## Use

```sh
edition status                         # lines, tags, what each channel and platform holds
edition plan 0.6.0                     # what the release needs; what Concordance says it carries
edition verify --online                # every invariant; exit 1 on an error

edition deploy staging-home main       # dry run: prints the plan and a receipt
edition deploy staging-home main --apply --confirm <receipt>

edition cut 0.6                        # release/0.6 from main (then apply with its receipt)
edition bump 0.6.0 --apply             # every version file (local only)
edition rc 0.6.0                       # v0.6.0-rc.1, rc.2, ... rc.10 (numbered numerically)
edition tag 0.6.0                      # v0.6.0
edition deploy production-home v0.6.0 --approved-by <name>
edition promote staging-home production-home   # what staging holds, if it is tagged

edition hotfix start 0.5 --apply       # hotfix/0.5.3 from release/0.5 (local only)
edition hotfix finish 0.5.3            # one plan: tag, publish, open the forward merge
edition forward                        # pull requests: release/* -> main

edition hooks install --apply          # pre-push: content check, versions, pins
edition ledger --channel staging-home
```

Every plan with outward steps refuses to run from a dirty tree or a detached HEAD, and
while the remote cannot be read. Read-only commands work offline and say the remote
state is unverified.

Exit codes: `0` success or a printed dry run, `1` refused or a failed step, `2` usage,
configuration or receipt error.

## Configuration

One `edition.toml` at the repository root. A short example:

```toml
product = "inkwell"

[[version_files]]
path = "Cargo.toml"
after = "[workspace.package]"
pattern = 'version = "{version}"'

[platforms.home]

[checks.test]
run = ["cargo", "test", "--workspace", "--locked"]

[channels.staging-home]
tier = "staging"
kind = "home"
target = "https://staging.example.com"
platform = "home"
checks = ["test"]
run = ["{var.deploy_script}", "release", "{commit}"]
probe = ["curl", "-fsS", "https://staging.example.com/healthz"]

[vars]
deploy_script = ""   # per machine: EDITION_VAR_DEPLOY_SCRIPT (printed by name only)

[remotes.origin]
public = true        # pushes are scanned for internal notes first
```

Commands are argument lists, run directly. A placeholder (`{commit}`, `{tag}`,
`{var.NAME}`, ...) must be a whole argument or environment value; `sh -c` is rejected.
Full examples for a fictional library with a home server and a fictional two-platform
app: [examples/library](examples/library/edition.toml) and
[examples/app](examples/app/edition.toml). Every key is described in
[docs/design.md](docs/design.md#configuration).

## Development

```sh
cargo test        # unit tests, and scenario tests against temporary git repositories
cargo clippy --all-targets
```

The scenario tests create bare repositories as remotes and never touch the network.
Dependencies are kept to `serde`, `serde_json` and `toml`, at exact versions with default
features off.

## License

Apache-2.0.
