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
- **Dry run first, always.** Every mutating command prints its plan. `--apply` runs
  local steps. A plan that leaves the machine (a push, a deploy, an upload, a pull
  request) runs only with `--confirm <receipt>`, where the receipt was issued by a dry
  run of exactly that plan (see Receipts). Nothing runs when the receipt does not match.
- **Live state, checked twice.** Outward plans are built from the remote's live heads and
  tags (`git ls-remote`), never from possibly stale remote-tracking refs, and each outward
  step re-reads the remote immediately before it runs.
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
- A hotfix starts on `hotfix/X.Y.Z` from the line (`hotfix start`, local only), lands on
  the line by review, and is finished by `hotfix finish X.Y.Z`: one plan with one receipt
  that tags the line, publishes the tag and opens the forward-merge pull request. The
  pull request step expects the tag it just published, so the plan never leaves a public
  tag without its forward merge unless a step fails, which the ledger then records.
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
| `prod` | a release tag named explicitly, on its release line and present on the remote; every pin on a final tag | if the channel says `owner` or `touch` | recorded |

- **Staging tracks `main`.** Every merge to `main` is deployable to every staging
  channel without further approval, and nothing else is: not an older `main` commit, not
  a branch. "The tip" is the remote's live head, read when the plan is made and again
  just before the deploy command runs. This applies alike to a staging server, TestFlight internal testing and Play
  internal testing.
- **Production takes releases.** A prod channel takes a tag named explicitly
  (`refs/tags/v1.4.0`, or `v1.4.0` when no branch has that name); a branch or a bare
  commit is refused even when a release tag points at it. The tag must be on the remote,
  pointing at the commit Edition resolved, and on its release line, and every pinned
  dependency must itself be on a final tag (see Pins).
- **Release candidates** are prod-tier refs only for channels that set
  `allow_prerelease`, which configuration accepts only on review or testing kinds
  (`testflight`, `play-testing`, `store-review`, `beta`). A production server cannot set it.
- **Approvals.** `approval = "owner"` requires `--approved-by <name>`, which is recorded.
  `approval = "touch"` also notes that the command itself asks for a hardware key. Only
  prod channels may ask for an approval; staging never does.

### Receipts

A dry run of a plan with outward steps prints a **receipt**: a random 128-bit nonce from
the operating system's random source. Edition stores, under `.edition/` beside the
configuration (a mode-0700 directory with a mode-0600 file, never committed, refused if
group or others can read it), the nonce with the SHA-256 of the plan's canonical text, the
issue time and a fixed expiry. The canonical text contains the configuration file's
digest, every fact the plan was checked against (resolved commits, tags, version, build,
approver, pins, allowed content findings), and for every step its kind, resolved program
path, arguments, the complete environment it will run with (values included, so a changed
value changes the hash, though values are never printed), working directory and remote
expectations. Timestamps and notes are not part of it.

`--apply --confirm <receipt>` recomputes the plan from scratch, then, holding one
exclusive lock on the receipts file across all of it, checks that the receipt was issued
here, has not expired, has not been used (locally or in the ledger), and was issued for a
plan with the same hash, and marks it used. Only then does the first step run. A crash
after that leaves the receipt spent: run the dry run again.

- The lifetime is 30 minutes, configurable as `[receipts] ttl_minutes` up to 60. There is
  no clock or lifetime override in the environment.
- `O_NOFOLLOW` and `O_DIRECTORY` come from a per-target table (macOS; Linux x86_64,
  aarch64 and arm) so no `libc` dependency is needed; any other target fails to compile.
- The directory is opened once without following links and checked through the handle
  (a directory, mode 0700); the file is opened relative to that path with `O_NOFOLLOW` and
  checked through its handle (a regular file, mode 0600, same owner as the directory);
  then the directory path is checked to still name the same directory. Without `openat`
  in the standard library a narrow window remains between the directory check and the
  file open, which the final identity check detects in most cases.
- File modes protect against other users only. Any process running as the same user can
  read and write the state, as it can the repository itself; receipts guard against
  mistakes, stale plans and replays, not against a hostile process with your identity.
- There is no way to run an outward step without a receipt from a prior dry run of the
  same plan.

### Remote state

Plans with outward steps read the remote with `git ls-remote` and use those heads and
tags. If the remote cannot be read, the plan is refused; if a live head is not in the
local clone, Edition asks for a fetch rather than reasoning about a commit it cannot see.
Each outward step also carries expectations, printed with the plan, such as "remote
`refs/heads/main` is `a1b2c3`" or "`refs/tags/v1.4.0` is absent", and immediately before
the step runs the remote is read again. If any expectation fails, the step and everything
after it is skipped and the ledger records the run as `failed` with the reason.

Where git can compare and swap, the push itself enforces the expectation too: branch
creation and tag pushes use `git push --atomic --force-with-lease=<ref>:`, which the
remote refuses if the ref appeared in the meantime. Deploy commands receive the verified
commit SHA, never a ref name (`{ref}` and `{tag}` are not available to commands), so a
deploy script must deploy by SHA. The one remaining gap is the forward-merge pull
request, which names a branch that can move after it is opened; Edition records the head
SHA it checked in the ledger entry and in the pull request body.

Outward plans also refuse a dirty working tree and a detached HEAD, so the configuration
and version files are exactly what was committed. A branch at the same commit as the
remote tip is enough.

### Inputs that name files or refs

- `--confirm` must be exactly 32 lowercase hex characters, checked when the arguments
  are parsed, before the value is used for anything. A refused receipt creates and
  removes nothing.
- Receipt state, pending records and `local.toml` live at fixed names under the private
  `.edition/` directory; pending file names come only from validated receipts, and
  Edition removes only regular files it created there.
- `ledger`, `version_files` paths and pin files must be relative paths inside the
  repository (no `..`, not absolute): `bump` writes version files and `ledger migrate`
  reads the legacy ledger. Every component of such a path is checked with `lstat` from
  the repository root and a symbolic link anywhere refuses it; the file is opened with
  `O_NOFOLLOW`; `bump` writes a new file beside the old one (same mode) and renames it
  into place, so no write passes through a link. Before and after creating the new file,
  and just before and just after the rename, it re-checks the path and compares the
  parent directory's and the new file's device and inode; a change before the rename
  aborts and removes the new file, and a change seen after it is reported. **Residual:**
  without `openat`/`renameat` the parent is named by path at each step, so a process that
  can write to that directory can still swap it in the moments between the last check and
  the rename; Edition detects that afterwards but cannot prevent it. Concurrent writers to
  the working tree are outside this boundary. The content scan
  reads a tracked link as its target text and never follows it.
- Ledger validation accepts only mode 100644 for `ledger.toml` and rejects any other diff
  metadata (mode changes, renames, copies, deletions, binary changes).
- `ledger_branch`, refs and `--at` pass the ref grammar; the ledger branch cannot be
  `main` or a release line.
- `EDITION_EVENTS` is the caller's chosen output path; Edition only appends to it and
  never writes through a link.

### Commands, placeholders and secrets

- Channel and check commands are argument lists executed directly, never through a
  shell; `sh -c` (and the other common shells with `-c`) is rejected when the
  configuration loads. A pipeline belongs in a script file.
- A placeholder (`{commit}`, `{version}`, `{build}`, `{channel}`, `{target}`,
  `{var.NAME}`) must be a whole argument or environment value. Text such as
  `--sha={commit}` is rejected.
- Inputs are checked against strict grammars before use: refs and `--at` (letters,
  digits, `.`, `_`, `/`, `-`; no leading `-`, no `..`), build labels (letters, digits,
  dots), versions (`MAJOR.MINOR.PATCH[-pre]`), channel names (lowercase, digits,
  hyphens), actor names (one printable line). Machine values may not start with `-`.
- A `[vars]` value set to `""` must come from `EDITION_VAR_NAME` on the machine. Such
  values are treated as secret: plans print `<var.NAME>`, environment variables are
  printed by name only, and the ledger records each step's configured template
  (`{var.NAME}`), never expanded values.
- Configured commands (channel `run` and `probe`, and checks) run with a **cleared
  environment**: `PATH` (from `[run] path`, or `/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin`),
  `HOME`, `LANG`, `TMPDIR`, and the variables the channel or check declares in `env`.
  Nothing else is inherited, so a script that needs a credential must declare the
  variable (for example `env = { DEPLOY_TOKEN = "{var.deploy_token}" }`). The program is
  resolved to an absolute path when the plan is made, and that path is part of the plan
  hash.

### Per-machine PATH

A tool that lives in a directory only one machine has (for example `~/.local/bin`) is
added in `.edition/local.toml`, which is never committed:

```toml
[run]
path_prepend = ["~/.local/bin"]
path_append = []
trust_group_writable = []
```

The entries follow exactly the same directory rule as `[run] path`, with this file's
`trust_group_writable` added to the committed one; a failing entry refuses the
configuration. The file and `.edition/` must be owned by the user, not be links, and not
be writable by group or others, and Edition refuses the file if git tracks it or does not
ignore it (Edition writes `.edition/.gitignore` when it creates the directory). Plans print `local path entries: ...`, and the entries
are part of the plan hash, so a receipt still binds what will run.

A file, rather than environment variables such as `EDITION_PATH_PREPEND`, because it is
persistent and visible per checkout, lives in the private state directory with the same
ownership checks as the receipts, and can carry its own trust list. Edition deliberately
distrusts ambient environment elsewhere (git, gh and configured commands all run with a
cleared environment); a PATH that any parent process could extend would undo that.

### git and gh

Edition's own git and gh calls, read-only or outward, are bound the same way:

- They run with a cleared environment: `PATH` (from `[run] path` or the default), `HOME`,
  `LANG`, `TMPDIR`, `SSH_AUTH_SOCK`, the git author and committer identity variables, and
  only those variables listed in `[run] tool_env` (such as `GH_TOKEN`,
  `GH_ENTERPRISE_TOKEN`, `GPG_TTY` or `GNUPGHOME`). `GIT_*` variables (including
  `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_COUNT`), `GH_HOST`, `GH_REPO`
  and `GH_CONFIG_DIR` can never be passed through, so `GIT_DIR`, `GIT_SSH_COMMAND` and
  the like in the caller's environment have no effect.
- **Trust base.** Git reads its default system file (for example Homebrew's
  `/opt/homebrew/etc/gitconfig`, or Apple's git's, where stock macOS setups keep
  `credential.helper = osxkeychain`), the global file chosen by `HOME`, the repository's
  own file and every file they include. Before any git operation that can contact a
  remote or run a helper (`status --online`, `verify --online`, and every outward plan),
  Edition enumerates that chain explicitly and refuses outright if any file in it is
  writable by its group or others or owned by a user other than root or you; it does not
  judge which settings in such a file are harmless. The chain is: the system, global and
  XDG files as `git var GIT_CONFIG_SYSTEM` and `GIT_CONFIG_GLOBAL` report them for the
  environment Edition gives git (reported whether or not the files exist; this needs
  git 2.42 or later, and Edition refuses when git cannot answer); the repository's
  `config`, and `config.worktree` when `extensions.worktreeConfig` is set; every file
  `git config --list --show-origin` names (which also catches files such as Apple git's
  built-in configuration); and, recursively, every `include.path` and `includeIf.*.path`
  target, parsed from the files themselves (`~/` from `HOME`, relative paths from the
  including file), so a file that contributes nothing yet is still checked. Every
  `includeIf` target is audited whatever its condition, so a later change of branch or
  directory stays inside the boundary. A target that does not exist yet is bound as
  missing, and refused if the nearest existing directory above it lets others create it.
  Cyclic includes, chains deeper than git's limit of 10 and unresolvable targets
  (`%(prefix)/...`) fail closed. Offline, `verify` reports the same problems as
  warnings.
- **Binding.** Every outward plan, including ones made only of notes such as
  `ledger reconcile`, binds the identity of each file in the chain (path, owner, mode,
  size, modification time and SHA-256 of its contents) and the settings that change how
  git reaches a remote, authenticates or signs (`url.*`, `credential.*`, `http.*`,
  `protocol.*`, `include.path`, `includeIf.*.path`, `gpg.program`, `gpg.*.program`,
  `gpg.format`, `tag.gpgSign`, `commit.gpgSign`, `push.gpgSign`, `user.signingKey`,
  `core.sshCommand`, `core.gitProxy`, `core.askPass`, `remote.*.url`, `pushurl`, `proxy`,
  `receivepack`, `uploadpack`). An edit to any file between dry run and apply
  invalidates the receipt. The plan prints the bound files and the names of the bound
  settings.
- Every git call forces `core.hooksPath=/dev/null`, `core.fsmonitor=false` and
  `protocol.ext.allow=never`, and names its subcommand explicitly.
- When a plan is made, the remote is resolved to its effective fetch and push URLs (after
  `insteadOf`, `pushInsteadOf` and `pushurl`). Reads, re-checks and pushes use those URLs,
  never the remote's name, and the URLs are part of the plan hash: a configuration change
  that would redirect the remote, even to a mirror with identical refs, changes the plan
  recomputed at apply time and the receipt is refused.
- gh is called with `--repo <host>/<owner>/<name>` and `GH_HOST` derived from the remote's
  effective URL (or, when git rewrites it to a local path, its configured URL); a plan
  whose URL does not name a forge repository is refused.
- The plan shows the destination of every outward step.
- URLs are kept verbatim only for hashing and execution. Redaction is structural: every
  line Edition prints goes through one redacting printer, every ledger entry is redacted
  field by field when it is appended, and every event value is redacted, so userinfo
  (`user:token@`) and query strings never appear in plans, titles, status, errors, git's
  own output from Edition's steps, the ledger or the event stream, wherever in the
  configuration the URL came from. Output that configured commands write themselves goes
  straight to the terminal.

The trusted base is the user's global git configuration and what it names: credential
helpers, ssh configuration and `includeIf` files. Edition does not and cannot vet those.

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

The ledger is a file, `ledger.toml`, on its own orphan branch (`ledger_branch`, default
`edition/ledger`), never in the product's history. Keeping it out of `main` matters: the
staging tier accepts exactly the tip of `main`, so a ledger committed to `main` would move
`main` with every recorded deploy, leave staging permanently one commit behind, and start
CI for every record.

It is an append-only list of `[[entry]]` tables, one per outward action: `deploy`,
`push-branch`, `push-tag`, `open-pr`. Each entry records the time, the actor, the
channel and tier, the ref and commit, the tree hash, the version, platform and build, the
result, the receipt, the approval, the step's configured command (never expanded
values), every step that ran with its exit code, gate evidence, every pin with its tag,
and any content findings explicitly allowed. Entries are redacted before they are
written.

- **Writing.** Every plan that records ends with one more outward step, printed with the
  plan and bound in its receipt: publish the outcome on the ledger branch of the same
  remote. Edition builds the commit with plumbing (`hash-object`, `mktree`,
  `commit-tree`), so the checkout is never touched, and pushes it with
  `--force-with-lease` on the tip it read. If another recorder won the race, Edition
  re-reads the branch, re-chains the entry and tries again (up to eight times). An entry
  whose id is already on the branch (a push that was accepted but whose answer was lost)
  counts as published and is never appended twice; ids embed the run's receipt. A failed
  or partial action is recorded the same way.
- **Pending outcomes.** Once the receipt has been validated and spent, and before the
  first step of a recording plan runs, Edition writes the prospective entries to
  `.edition/pending/<receipt>.toml` (a private directory, files mode 0600, written
  atomically without following links; the name comes only from the validated receipt),
  rewrites them after every step, and removes them once the ledger
  push succeeds. A step that had not finished when Edition stopped is recorded as
  `unknown`. If Edition is killed, or the remote cannot be reached or the entry is
  blocked by the content check, the outcome survives: `status` and `verify` report it as
  unreconciled, every new outward plan is refused, and `edition ledger reconcile` (a dry
  run, then the receipt) publishes it without running anything again;
  `reconcile --abandon` records it as `abandoned` instead.
- **Results.** `ok`: the step and everything after it succeeded. `partial`: the outward
  step succeeded and a later step (a probe, a Concordance record) failed, or a later
  outward step was stopped. `failed`: the outward step failed, a check before it failed,
  or a remote expectation stopped it.
- **Hash chain and format.** The file starts with `format = 2`. Each entry's `prev` is
  the SHA-256 of the previous entry's canonical text (64 zeros for the first), computed
  by Edition itself, so the chain is the same in SHA-1 and SHA-256 git repositories. A
  broken chain is never extended, and a file without the marker or with another format
  is refused. Format 1 (no marker, chained with the repository's `git hash-object`) is
  converted by `edition ledger migrate`, which re-chains the entries (keeping everything
  else) and appends an entry recording the migration.
- **History.** Each commit on the branch appends one entry; `verify` checks the chain and
  that along the branch's first-parent history every version is a prefix of the next,
  naming the first commit that edited, removed or reordered an entry.
- **Anchor and validation.** Edition keeps the last remote tip it saw in
  `refs/edition/ledger`. Every outward plan and `verify --online` read the remote branch
  and refuse a tip that does not descend from it (a force push, a deletion). When the tip
  has moved, every new first-parent commit is validated as an exact one-entry extension
  of its parent: the tree holds only `ledger.toml`, the text is the parent's text plus
  one rendered entry, the entry chains to the one before and repeats no id, and a merge
  or a second root is refused. A branch seen for the first time is validated from its
  first commit, which must start at the genesis value. A missing or unreadable file fails
  closed, and the refusal names the commit. Validation reads one `git log -p` stream and
  checks each commit's appended lines against a running chain state, so its cost is
  linear: a first sync of a 5,000-entry branch took about 4.6 s in a debug build on an
  Apple-silicon laptop (`validation_cost_for_5000_entries`, run with `--ignored`). Offline, `verify` says the anchor is
  unverified. The anchor is only as strong as the remote: protect the ledger branch
  against force pushes and deletion.
- **Reading.** `status`, `ledger`, `promote` and the `requires` gate read the branch
  (synchronised first when the remote is read, as it always is for outward plans).
- Dev-tier deploys are never recorded.
- `verify` checks every successful entry: its commit is reachable from the remote; a
  staging entry's commit is on `main`; a prod entry's ref is a release tag at that commit.
- "What is deployed where" is the latest successful entry per channel; `status` also says
  whether each staging channel is current with `main`.
- **Migration.** A repository with an in-tree ledger file (`ledger`, default
  `releases/ledger.toml`) gets a warning from `verify`; `edition ledger migrate` (a dry run
  first, then the receipt) publishes it as the ledger branch, one commit per entry, with a
  push that requires the branch not to exist. Remove the file afterwards in an ordinary
  commit.

### Content check

Remotes may be marked `public = true`. What would be published to one is scanned for a
deny-list: by default notes addressed to a named person, unanswered to-do markers,
home-directory paths and private-key headers. The scan covers every commit reachable
from what is pushed and from nothing the remote already has: commit messages and every
added line, with renames shown as a deletion plus an addition (so a file moved out of an
exempt path is scanned in full) and merges as combined diffs (a line counts as added by a
merge only when it is new against every parent).

The scan fails closed: if git cannot produce the history, the plan, push or `verify`
fails. Content it cannot read as text (binary files, and in the tree files over 4 MB) is
reported as an `unscanned` finding, which blocks like any other unless its path is listed
in `[content] allow_paths` or it is allowed by id for one run.

- **Edition's own plans.** Every plan that pushes to a public remote runs the scan when
  the plan is made, and findings refuse the plan. A finding can be let through only by
  naming its id with `--allow-finding <id>`; the id becomes part of the receipt and is
  recorded in the ledger. (Edition's pushes publish commits the remote already has, plus
  the annotated tags Edition writes, so in practice this scan is a backstop.)
- **Every other push.** `edition check-push <remote>`, installed as a pre-push hook by
  `edition hooks install`, runs the same scan on the refs git is about to push and refuses
  the push on a finding (`content.action = "block"`, the default; `"report"` only
  prints). It also reports, without blocking, disagreeing version files and pins that are
  not on a release tag.
- **`verify`** scans the tracked tree and commits not yet on each public remote, and
  fails on a finding when the action is `block`.
- **The ledger branch** is pushed to the same remote. Its entries carry configuration
  and user text (targets, notes), so for a public remote the prospective entry is
  scanned while the plan is made (for every recording plan and for `ledger reconcile`).
  A finding refuses the plan unless it is allowed by id with `--allow-finding`; allowed
  findings are printed with the plan, part of its hash, and recorded in the entry. The
  final entry is scanned again before the push, and a finding that was not allowed when
  the plan was made blocks it; the outcome stays pending. At that point an id given on
  the command line counts for nothing: only the allowances bound in the receipt do. The
  entry's own `overrides` list (which quotes the allowed patterns) is not scanned.

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
- **An attention board or agent runner** can follow the event
  stream below instead of polling repositories.

## Worker contract

With `EDITION_EVENTS` set to a file path (or `-` for standard error), Edition appends one
JSON object per line for each event:

| `event` | When | Fields |
| --- | --- | --- |
| `channel-updated` | a deploy was recorded (ok or failed) | `id`, `channel`, `ref`, `commit`, `result`, `by` |
| `ref-published` | a pushed branch or tag, or an opened pull request, was recorded | `id`, `ref`, `commit`, `result`, `by` |
| `gate-failed` | a gate refused a deploy | `gate`, `channel`, `reason` |
| `refused` | any plan was refused | `reason` |
| `parity-gap` | the parity gate found an item that is not ready | `release`, `item` |

Every event carries `at` (UTC). Consumers treat the file as append-only and keep their own
offset. Edition never reads it back.

## Configuration

| Key | Meaning |
| --- | --- |
| `product`, `remote`, `main` | product name; the remote (default `origin`); the main branch |
| `ledger_branch` | the orphan branch that holds the ledger (default `edition/ledger`) |
| `ledger` | where an in-tree ledger from before the ledger branch lives, for `edition ledger migrate` (default `releases/ledger.toml`) |
| `[tags] final`, `prerelease`, `prerelease_files` | tag templates (`v{version}`, `v{version}-rc.{n}`); what version files say at a candidate (`base` or `full`) |
| `[lines] branch`, `dev_suffix`, `hotfix_branch` | `release/{line}`; `dev`; `hotfix/{version}` |
| `[[version_files]] path`, `pattern`, `after` | where the version is stated |
| `[platforms.P] build_file`, `build_pattern`, `concordance` | per-platform build number; the platform's name in Concordance |
| `[checks.C] kind`, `run`, `env`, `note` | `command` or `parity` |
| `[channels.C] tier`, `kind`, `target`, `platform`, `checks`, `requires`, `approval`, `allow_prerelease`, `run`, `env`, `probe`, `note` | a place a build can go |
| `[repos.R] path`, `remote`, `final_tag` | another repository that pins point into |
| `[[pins]] name`, `file`, `pattern`, `repo` | a pinned dependency |
| `[remotes.R] public` | marks a remote for the content check |
| `[content] deny`, `allow_paths`, `action` | the content check's patterns (`<word>` matches one word), exempt path globs, `block` (default) or `report` |
| `[concordance] catalog`, `release`, `bin` | the Concordance catalog; the release id template (`{version}`) |
| `[vars] NAME` | values for `{var.NAME}`; an empty value must come from `EDITION_VAR_NAME` on the machine and is treated as secret |
| `[run] path` | `PATH` for every command Edition runs, including git and gh: absolute directories, or under `~/` (which needs an absolute, colon-free `HOME`); each is canonicalized and missing ones are dropped. The entry and every ancestor up to `/` must be owned by root or the current user; none may be writable by others (an ancestor may be, if sticky); none may be group-writable (an ancestor may be, if sticky) unless its exact canonical path is in `trust_group_writable`. A configured entry that fails refuses the configuration; the built-in default skips such directories, and a program that is then not found is reported with the skipped group-writable directories |
| `[run] trust_group_writable` | exact canonical paths of group-writable directories to allow in `path` (for example `/opt/homebrew/bin`); each one in use is printed with the plan and part of its hash |
| `[run] tool_env` | variables git and gh may inherit, such as `GH_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GPG_TTY`, `GNUPGHOME` |
| `[receipts] ttl_minutes` | receipt lifetime, 1 to 60 (default 30) |
| `[verify] tags_since` | older tags are not checked against version files |

Placeholders in `run`, `probe`, `env` and check commands, each a whole argument or value:
`{commit}` (the verified SHA), `{version}`, `{build}`, `{channel}`, `{target}`,
`{var.NAME}`. Environment variables Edition reads: `EDITION_ACTOR`, `EDITION_EVENTS`,
`EDITION_VAR_<NAME>`.

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
