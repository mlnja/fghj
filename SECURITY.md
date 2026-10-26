# Security policy

`fghjd` runs as root, trusts a local root CA, and edits system networking
config, so security reports get priority over everything else.

## Reporting a vulnerability

Please report privately through GitHub's
[private vulnerability reporting](https://github.com/mlnja/fghj/security/advisories/new)
rather than in a public issue. Include the fghj version (`fghj --version`),
your macOS version, and the steps to reproduce.

You should get a first response within a few days. Fixes ship as a new
release, and the advisory is published once that release is out.

## Supported versions

Only the latest release gets security fixes. fghj is pre-1.0 and releases
often, so upgrade with `brew upgrade fghj`.

## What is in scope

Anything that lets a process, a container, or a website do more than it
could without fghj installed. For example:

- getting a certificate trusted for a name outside fghj's local zones;
- reading the root CA's private key without root;
- reaching the control socket or API without the access it should require;
- a container escaping its run's network through the sidecar or proxy;
- changes to `/etc/hosts`, `/etc/resolver`, `pf` or the keychain beyond what
  [What fghj touches](https://fghj.mlops.ninja/guides/uninstalling/) lists.
