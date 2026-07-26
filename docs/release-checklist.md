# Release checklist

- [ ] Cargo version updated
- [ ] Changelog updated
- [ ] CI green
- [ ] Release validation script passes
- [ ] Installer tests pass
- [ ] Platform assets match installer
- [ ] SHA256SUMS generated
- [ ] Temporary install succeeds
- [ ] Temporary-project smoke test succeeds
- [ ] README version/platform claims correct
- [ ] No secrets in artifacts
- [ ] Tag reviewed

After reviewing the exact commit and confirming every item, the maintainer may run:

```bash
git tag v0.2.0
git push origin v0.2.0
```

Do not tag until `Cargo.toml` contains `version = "0.2.0"`. A manual run of the
release workflow builds and validates artifacts without publishing. A pushed
matching version tag is required for publication.
