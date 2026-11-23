# Edition

Release lines, tags, channels and a deploy ledger for a small shop that ships a library,
servers and apps from several repositories, much of it written by agents.

Edition answers three questions from files in the repository, not from memory:

- **May this ref go there?** Every channel (a staging server, a TestFlight group, a Play
  track, a production server) has a tier, and the tier decides which refs it accepts.
  The decision is made against the remote's live heads and tags.
- **What exactly will happen?** Every mutating command prints the plan first. A plan
  that leaves the machine runs only with the **receipt** its dry run issued: a random
  nonce bound to a SHA-256 of the plan, valid once, for 30 minutes, and only while the
  refs, the configuration, the environment and the remote are exactly as they were
  checked. Pushes are compare-and-swap, and deploy commands get the verified SHA.
- **What is deployed where?** Each deploy, pushed branch or tag, and forward-merge pull
  request is appended to a hash-chained ledger on its own branch (`edition/ledger`, so
  recording never moves `main`), with the commit, the
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
edition ledger migrate                 # move an in-tree ledger file to the ledger branch
edition ledger reconcile               # publish an outcome a crash or outage left pending
edition --version
```

Every plan with outward steps refuses to run from a dirty tree or a detached HEAD, and
while the remote cannot be read. Read-only commands work offline and say the remote
state is unverified.

Configured commands run with a cleared environment (`PATH`, `HOME`, `LANG`, `TMPDIR`
and the variables a channel declares), so a deploy script that needs a credential must
declare it, and must deploy the commit SHA it is given. Edition's own git and gh calls
also run with a cleared environment, with repository hooks off, and push to the remote
URL resolved when the plan was made; the plan shows every destination. The ledger's remote anchor is the
ledger branch: protect it against force pushes. Per-machine tool directories go in the
git-ignored `.edition/local.toml` (`[run] path_prepend`, `path_append`), under the same
directory rules, and are shown in every plan.

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

Commands are argument lists, run directly. A placeholder (`{commit}`, `{version}`,
`{var.NAME}`, ...) must be a whole argument or environment value; `sh -c` is rejected.
Full examples for a fictional library with a home server and a fictional two-platform
app: [examples/library](examples/library/edition.toml) and
[examples/app](examples/app/edition.toml). Every key is described in
[docs/design.md](docs/design.md#configuration).

## Authentication

Edition pushes and reads with git and gh as you have set them up:

- HTTPS remotes use git's configured credential helper, from the system, global or
  repository configuration (for example `osxkeychain` from Homebrew's or Apple's git).
- SSH remotes use your ssh agent through `SSH_AUTH_SOCK`, which Edition passes through.
- gh uses its stored login, or a token variable you declare in `[run] tool_env` (such as
  `GH_TOKEN` or `GH_ENTERPRISE_TOKEN`).

Before git contacts a remote, Edition refuses if any git configuration file it would read
is writable by group or others or owned by another user, and every plan binds those files
so an edit between dry run and apply invalidates the receipt.

## Residual risks

- Same-UID access to receipt state: any process running as you can read and write
  `.edition/`, as it can the repository.
- Trust in your system and global git configuration, ssh configuration and credential
  helpers, which Edition uses as they are.
- Pull-request heads are mutable: Edition records the head SHA it checked, but the branch
  can move after the pull request is opened.
- Ledger anchoring depends on a protected remote ledger branch.
- Locking is POSIX advisory locking, which some network filesystems do not honour.
- Signed-tag setups may need `GPG_TTY` or `GNUPGHOME` declared in `run.tool_env`.
- Output printed by configured channel commands is passed through to the terminal as-is;
  Edition does not redact it.
- Trusting a group-writable directory (`[run] trust_group_writable`) lets any member of
  that group replace the tools Edition runs.
- Pending outcomes (`.edition/pending/`) are editable by processes running as the same
  user.
- A crash can leave a step recorded as `unknown`, which needs operator judgment at
  `edition ledger reconcile`.
- Concurrent writers to the working tree are outside the filesystem-safety boundary: the
  configured-path checks are not atomic.

## Development

```sh
cargo test        # unit tests, and scenario tests against temporary git repositories
cargo clippy --all-targets
```

The scenario tests create bare repositories as remotes and never touch the network.
Dependencies are kept to `serde`, `serde_json` and `toml`, at exact versions with default
features off. There is no `libc` dependency: the two `open(2)` flags Edition needs come
from a per-target table (macOS; Linux x86_64, aarch64 and arm), and other targets fail to
compile.

## License

Apache-2.0.
