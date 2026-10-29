# Security

gtron-node holds the private key of a wallet with staked TRX. This file says what it
protects against, what it does not, and how to report a problem.

## What the agent guarantees

- **The key stays here.** It is read from a file (mode 0400, owned by the agent's user) or
  from the environment, never from a command-line flag, and is not sent anywhere: the
  platform only ever receives transaction ids and, during setup, a signed verification
  message.
- **It signs only what the job says.** The TRON node builds each transaction; the agent
  decodes it (`crates/tron-signing/src/txcheck.rs`) and signs only if it is exactly the
  expected `DelegateResource` / `UnDelegateResource` from this wallet, to the job's
  receiver, for the job's amount and lock. A lying node cannot slip in a transfer.
- **Its own limits, whatever the platform asks.** Per delegation, per 24 hours, in total
  locked, longest lock, and an optional receiver allowlist (`crates/node/src/limits.rs`).
  They bound what a compromised platform account could make this wallet do.
- **One signature per job.** A signed transaction is written to the state directory before
  it is broadcast; a retry re-sends the same transaction and never signs a second one.
- **Setup signs one kind of message.** Wallet verification signs only a message that starts
  with `gtron.network address verification` and names this very wallet, after showing it.

## What it does not protect against

- Someone who can read the key file (root on the machine, a backup that copies it).
- A state directory that is deleted: the agent then no longer knows what it signed.
  Delegations are bounded by the limits above, not by the state.
- Delegations you make with the same wallet outside the agent: they do not count towards
  its limits.

## Verifying a release

Every release lists `SHA256SUMS` for its archives; `install.sh` refuses an archive that does
not match. The binaries are built by the `release` workflow in this repository from the
tagged source.

## Reporting

Use **Security → Report a vulnerability** in this repository (a private advisory only the
maintainers see), with what you found and how to reproduce it. Please do not open a public
issue for a vulnerability.
