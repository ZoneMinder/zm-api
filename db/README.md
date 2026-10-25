# ZoneMinder schema files

The `*.sql` files in this directory are **vendored, unmodified copies from the
[ZoneMinder](https://github.com/ZoneMinder/zoneminder) project** and remain licensed
under ZoneMinder's **GPL-2.0** license — not under zm-api's AGPL-3.0 / commercial license.

They are the schema fragments that `zm_create.sql.in` pulls in via `source` directives.
The test-database setup scripts (`scripts/setup-ci-db.sh`, `scripts/db-manager.sh`)
resolve those directives by inlining these files when building the test schema.

| File | Purpose |
|------|---------|
| `Object_Types.sql` | `Object_Types` table |
| `User_Preferences.sql` | `User_Preferences` table |
| `triggers.sql` | Event_Summaries / rollup maintenance triggers (13 MySQL triggers) |
| `manufacturers.sql` | Camera manufacturer seed data |
| `models.sql` | Camera model seed data |
| `controls.sql` | `Controls` (PTZ/control capability) seed rows — moved out of `zm_create.sql.in` upstream at 1.39.3x |
| `AI_Models.sql` | AI detection tables (`AI_Datasets`, `AI_Models`, `AI_Object_Classes`, `AI_Detection_Settings`, `AI_Detections`) |
| `coco_dataset.sql` | COCO object-class seed data |

`zm_config.sql` is generated, not vendored: `scripts/generate-zm-config.sh` runs
ZoneMinder's own `zmconfgen.pl` over its `ConfigData.pm` and keeps the `Config`
rows a fresh install gets (the packaged `zm_create.sql` has them; the `.in` file
doesn't). The setup scripts append it after the schema so the test database has a
real `Config` table. Its option text comes from ZoneMinder, so the GPL-2.0 note
above applies to it too. Regenerate it when the schema mirror moves to a new
ZoneMinder version.

These files, together with `../zm_create.sql.in`, are also the input to
`scripts/gen_baseline_migration.py`, which generates the baseline SeaORM
migration (`src/migration/m00000000_000001_zm_baseline/`).

Source: <https://github.com/ZoneMinder/zoneminder/tree/master/db>
