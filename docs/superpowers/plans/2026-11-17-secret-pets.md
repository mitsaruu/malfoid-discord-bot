# Secret pets implementation plan

Approved balance: secret case 25000 coins; epic 60%, legendary 34.95%, mythic 5%, secret 0.05%. Only secret case contains secret rarity; sole secret pet is Стеллочка (1500/hour). Фоидка романа is mythic (550/hour). User correction: Даня собака is legendary (170/hour).

- [x] Regression tests: case defaults/config override, exact odds boundaries, sole secret species and exclusive availability, all four distributions total 100, choice order compatibility, new species income and existing species persistence.
- [x] Extend case/rarity tables to four cases, append new choices and species without changing existing keys or indexes. Include secret rarity in sale confirmation and correct rarest-pet caption.
- [x] Update `/cases` catalogue, config, docs and patchnotes. Verify catalogue embed limits.
- [x] Run relevant Rust tests in isolated Linux workspace without keys and review diff. Existing uncommitted Hydrogen fix stays intact; no push/deployment without request.

Verification: isolated Linux cargo test --release --locked -j 1 passed: 212 tests, 6 ignored. Boundary/exclusivity and command choices/catalogue tests passed. Read-only review found no concrete defects. No real Discord opening/selling performed; no commits, push or production rollout.
