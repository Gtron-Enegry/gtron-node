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
- **Setup signs one kind of message.** Wallet verification signs only the platform's exact
  ownership message for this very wallet (`is_verification_message` in
  `crates/node/src/setup/platform.rs`), after showing it.
- **Locks are counted as the chain keeps them.** A new locked delegation to a receiver
  extends the lock of everything already locked to it; the locked limit counts it that way.
- **A delegation-only key cannot move TRX.** With `NODE_WALLET_ADDRESS` and
  `NODE_PERMISSION_ID` the agent signs under an active permission of the wallet, and refuses
  to start unless that permission allows `DelegateResource` and `UnDelegateResource` and
  nothing else.
- **Bounded input.** Answers from the platform and the TRON node are capped at 1 MB,
  redirects are not followed, and text from them is stripped of terminal escapes before it
  is printed.

## What it does not protect against

- Someone who can read the key file (root on the machine, a backup that copies it). With the
  wallet's own key they control the wallet; with a delegation-only key (recommended) they
  can only delegate and reclaim resources, never move TRX.
- A state directory that is deleted: the agent then no longer knows what it signed.
  Delegations are bounded by the limits above, not by the state.
- Delegations you make with the same wallet outside the agent: they do not count towards
  its limits.

## Verifying a release

Every release lists `SHA256SUMS` for its archives; `install.sh` refuses an archive that does
not match. The binaries are built by the `release` workflow in this repository from the
tagged source, and every file and the Docker image carry a GitHub build provenance
attestation:

```sh
gh attestation verify gtron-node-x86_64-unknown-linux-musl.tar.gz -R Gtron-Enegry/gtron-node
```

`install.sh` runs that check itself when the GitHub CLI is installed and signed in.

## Reporting

Use **Security → Report a vulnerability** in this repository (a private advisory only the
maintainers see), with what you found and how to reproduce it. Please do not open a public
issue for a vulnerability.
