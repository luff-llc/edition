# Edition: design

## What it is

Edition is a small command-line tool that keeps a repository's releases in order: which
branches carry which versions, which tags mark releases, which builds may go to which
places, and a record of what went where. It is meant for a shop where one person owns the
decisions and many agents do the work, across a library, the servers that run it and the
apps that embed it.

It exists because release mistakes in such a shop are rarely dramatic. They look like an
SDK commit that an app shipped against but that exists only in one local checkout; a fix
made on a release branch that never reached `main`; a staging server running a branch
nobody can name; a build number that differs between the iOS and Android builds of the
same release. Each is easy to avoid once it is checked, and easy to miss when it is not.

## Principles

- **Orchestrate, do not reimplement.** Signing, uploading, deploying and secrets stay with
  the scripts that already do them well. A channel names the command that performs it;
  Edition decides whether it may run, runs it at the checked commit, and records it.
- **Dry run first, always.** Every mutating command prints the exact commands it would
  run. `--apply` runs the local ones. Anything that leaves the machine (a push, a deploy,
  an upload, a pull request) also needs `--confirm <token>` for that run, and the token
  is printed by the dry run. A missing confirmation is caught before any step runs, so a
  plan never stops half done for that reason.
- **Policy is built in.** The three tiers are part of the tool, not of each channel's
  configuration. A channel declares its tier; it cannot loosen it.
- **Readiness, not dates.** A release moves forward by passing gates whose evidence is
  recorded. A target date is information for people; Edition never reads one.
- **One version, many platforms.** A release is one version number across iOS, Android,
  servers and SDKs. Build numbers are per platform.
- **Files over services.** Configuration, the ledger and the events are plain files in
  the repository or on the machine. There is no daemon and no server.
- **One plan, kept elsewhere.** What a release carries is stated once, in Concordance's
  tracker. Edition reads it and records facts back through Concordance's own CLI; it
  keeps no feature list of its own.

## What it is not

- Not a CI system or a build tool. It runs checks you name; it does not schedule them.
- Not a deployer. It has no SSH, store or cloud client. The channel's command does that.
- Not a secrets manager. It never reads keys; hardware approvals stay in the tools that
  ask for them.
- Not a planner. Releases, their contents and their state live in Concordance.
- Not a history rewriter. Its content check reports; it never edits commits.

## The model

### Lines and versions

- `main` carries the next version and always builds as `X.Y.0-dev` (the suffix is
  configurable). `verify` fails when `main` builds as anything else, or when its version
  is not ahead of the newest release line.
- `release/X.Y` (the pattern is configurable) carries a shipped line. Its version files
  say the release it will tag next: `0.5.2`, then `0.5.3` after a hotfix.
- Lines merge forward into `main`. `main` is never merged into a line, and fixes are not
  cherry-picked backwards. `verify` lists every commit on a release line that `main`
  does not contain; `forward` offers one pull request per line.
- **Version files** are any number of files and patterns (`version = "{version}"`,
  optionally scoped to the first match after a given line). They must agree with each
  other at every commit Edition looks at, and with the tag at every release tag.
  `edition bump <version>` rewrites all of them.

### Tags

- `vX.Y.Z` marks a release; `vX.Y.Z-rc.N` marks a candidate. Both are made only on the
  line `release/X.Y`, and only when the version files there say `X.Y.Z` (or the full
  pre-release version, if `prerelease_files = "full"`).
- `rc` numbers candidates itself. `tag` refuses a version that is not newer than the
  line's latest release.

### Tiers and channels

A **channel** is a named place a build can go: a staging server, a production server,
TestFlight's internal group, a Play track, an app store. Each declares a tier, a kind,
a target, the platform it ships, the checks it needs, and the command that performs it.

| Tier | Accepts | Approval | Ledger |
| --- | --- | --- | --- |
| `dev` | any ref; runs on this machine | never | never recorded |
| `staging` | exactly the tip of `main` on the remote | never | recorded |
| `prod` | a final tag on its release line, present on the remote; every pin on a final tag | if the channel says `owner` or `touch` | recorded |

- **Staging tracks `main`.** Every merge to `main` is deployable to every staging
  channel without further approval, and nothing else is: not an older `main` commit, not
  a branch. This applies alike to a staging server, TestFlight internal testing and Play
  internal testing.
- **Production takes releases.** A prod channel takes a final tag that is on the remote
  and on its release line, and refuses it while any pinned dependency is not itself on a
  final tag (see Pins).
- **Release candidates** are prod-tier refs only for channels that set
  `allow_prerelease`: store review builds such as TestFlight external testing or Play
  closed testing. A production server never sets it.
- **Approvals.** `approval = "owner"` requires `--approved-by <name>`, which is recorded.
  `approval = "touch"` also notes that the command itself asks for a hardware key. Only
  prod channels may ask for an approval; staging never does.

### Gates

A deploy runs its checks before the channel's command and records their results. There
are three kinds of gate:

- **Command checks** (`[checks.NAME] run = [...]`): a test suite, a release-candidate
  gate script. The exit code is the evidence.
- **The parity gate** (`kind = "parity"`): Edition asks Concordance
  (`concordance track --format json`) for the release that matches the version and
  refuses while any item the release carries is not ready on the platforms the release
  builds. Concordance decides readiness; an accepted difference is recorded there (as
  `not-applicable`, or by moving the item out of the release), never in Edition.
- **Prior channels** (`requires = ["channel"]`): the ledger must show that the same
  commit already went to the named channel. Because `requires` compares commits, a
  production channel requires a prod-tier review channel (which saw the release
  candidate at the same commit), not a staging channel (which saw `main`).

### Platforms

`[platforms.NAME]` lists what the product ships: `ios`, `android`, `home`, `js`. The
version is shared; each platform may declare a build-number file and pattern. A channel
names its platform; `status` shows, for each tier, the version and build each platform
holds, side by side, from the ledger. When a Concordance catalog is configured, each
staging or prod deploy of a platform records the build with
`concordance log uploaded <release> --platform <p> --build <b> --commit <sha>`, keyed by
the ledger entry so a retry never records it twice.

### Pins

A repository that embeds another declares the pin: the file, a pattern with `{commit}` or
`{tag}`, and the repository it points into. `verify` resolves each pin in that
repository and fails when the commit is on no remote branch or tag, or is not the commit
of a final release tag, or (online) when that tag is not on the remote. A prod deploy
refuses on the same conditions, reading the pin file at the commit being deployed. This
is what makes "the app shipped against an SDK commit that exists only on one laptop"
detectable.

### The ledger

`releases/ledger.toml` (configurable) is an append-only list of `[[entry]]` tables, one
per outward action: `deploy`, `push-branch`, `push-tag`, `open-pr`. Each entry records
the time, the actor, the channel and tier, the ref and commit, the tree hash, the
version, platform and build, the result, the approval, the command that ran, the check
results, and every pin with its tag.

- Entries are never edited; a correction is a new entry. `verify` fails when the
  committed ledger is not a prefix of the working copy.
- Dev-tier deploys are never recorded.
- `verify` checks every successful entry: its commit is reachable from the remote; a
  staging entry's commit is on `main`; a prod entry's ref is a release tag at that commit.
- "What is deployed where" is the latest successful entry per channel.

The ledger is committed with the repository's other changes, through its normal review.

### Content check

Remotes may be marked `public = true`. Before anything is pushed to one,
`edition check-push <remote>` (installed as a pre-push hook by `edition hooks install`)
scans the commits being pushed, their messages and the lines they add, for a deny-list:
by default notes addressed to a named person, unanswered to-do markers, home-directory
paths and private-key headers. `verify` scans the tracked tree and unpushed commits the
same way. The default is to report; `content.action = "block"` makes the hook refuse the
push. The same hook also reports disagreeing version files and pins that are not on a
release tag.

## How it fits with other tools

- **Concordance** holds what each release carries, its state and its builds. Edition
  reads `concordance track --format json`, gates on it, and writes uploads back with
  `concordance log`. Edition's ledger holds what Concordance does not: which commit went
  to which channel, with which evidence.
- **Delivery scripts** (an app repository's archive-and-upload script, a Play publishing
  script, a home deployment kit) remain the only code that signs, uploads or deploys.
  Their own guards (a commit must be on `main`, a production home accepts only its
  pinned commit) stay in place; Edition's tier policy sits in front of them, and the two
  should agree.
- **Secrets** stay in whatever vault the delivery scripts use, behind their own
  approvals. Edition never sees them.
- **An attention board or agent runner** (Galley, for example) can follow the event
  stream below instead of polling repositories.

## Worker contract

With `EDITION_EVENTS` set to a file path (or `-` for standard error), Edition appends one
JSON object per line for each event:

| `event` | When | Fields |
| --- | --- | --- |
| `channel-updated` | a deploy was recorded (ok or failed) | `id`, `channel`, `ref`, `commit`, `result`, `by` |
| `ref-published` | a pushed branch or tag, or an opened pull request, was recorded | `id`, `ref`, `commit`, `result`, `by` |
| `gate-failed` | a gate refused a deploy | `gate`, `channel`, `reason` |
| `parity-gap` | the parity gate found an item that is not ready | `release`, `item` |

Every event carries `at` (UTC). Consumers treat the file as append-only and keep their own
offset. Edition never reads it back.

## Configuration

| Key | Meaning |
| --- | --- |
| `product`, `remote`, `main`, `ledger` | product name; the remote (default `origin`); the main branch; the ledger path |
| `[tags] final`, `prerelease`, `prerelease_files` | tag templates (`v{version}`, `v{version}-rc.{n}`); what version files say at a candidate (`base` or `full`) |
| `[lines] branch`, `dev_suffix`, `hotfix_branch` | `release/{line}`; `dev`; `hotfix/{version}` |
| `[[version_files]] path`, `pattern`, `after` | where the version is stated |
| `[platforms.P] build_file`, `build_pattern`, `concordance` | per-platform build number; the platform's name in Concordance |
| `[checks.C] kind`, `run`, `env`, `note` | `command` or `parity` |
| `[channels.C] tier`, `kind`, `target`, `platform`, `checks`, `requires`, `approval`, `allow_prerelease`, `run`, `env`, `probe`, `note` | a place a build can go |
| `[repos.R] path`, `remote`, `final_tag` | another repository that pins point into |
| `[[pins]] name`, `file`, `pattern`, `repo` | a pinned dependency |
| `[remotes.R] public` | marks a remote for the content check |
| `[content] deny`, `allow_paths`, `action` | the content check's patterns (`<word>` matches one word), exempt paths, `report` or `block` |
| `[concordance] catalog`, `release`, `bin` | the Concordance catalog; the release id template (`{version}`) |
| `[vars] NAME` | values for `{var.NAME}`; an empty value must come from `EDITION_VAR_NAME` on the machine |
| `[verify] tags_since` | older tags are not checked against version files |

Placeholders in `run`, `probe` and check commands: `{commit}`, `{ref}`, `{tag}`,
`{version}`, `{build}`, `{channel}`, `{target}`, `{var.NAME}`.

## Roadmap

Planned, in rough order. None of these is implemented yet.

1. **`edition ci`**: run the same checks as the pre-push hook and `verify` in a CI job,
   with annotations, and print a workflow snippet for GitHub Actions.
2. **Systems adoption check.** A repository declares the shared systems it uses (name,
   source repository, pin). `verify` reports a pin that is not a release tag on the
   remote (implemented today for pins), a declared system that is vendored or copied
   instead of imported (configurable path patterns), and a known system that is used
   but not declared. Reporting first; failing is opt-in per repository.
3. **`edition init`**: scaffold `edition.toml`, the hooks and a CI workflow for a new
   repository, so the same pipeline is cheap to adopt everywhere.
4. **Releases across repositories.** A release in one repository names the tags it
   depends on in others (an app release and the SDK tag it pins), and `plan` shows them
   together.
5. **Deployed-version probes.** Compare what a server reports about its own version with
   what the ledger says it holds, where the server exposes one.
6. **Release state from gates.** Record each gate's pass in Concordance's log as well,
   so a release's state there follows the evidence.
