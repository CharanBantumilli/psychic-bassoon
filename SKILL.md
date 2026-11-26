---
name: flagsmith-cleanup
description: Audit Flagsmith for stale or 100% rolled-out feature flags, verify zero remaining references or dead code paths in the codebase, open a cleanup PR via GitHub CLI, and archive the flag in Flagsmith. Use when asked to clean up feature flags, remove flag debt, or retire old toggles.
---

# Flagsmith Flag Debt Cleanup Workflow

Execute this end-to-end workflow to safely retire stale feature flags without bloating context tokens.

## Prerequisites & Guardrails
1. **Verify Auth & Context:** Run `flagsmith auth status --json --jq '{user: .email, org: .organisation.name}'` and `flagsmith config --json --jq '{project: .project_id, env: .environment}'`. If unauthenticated or uninitialized, stop and ask the user to run `flagsmith login` or `flagsmith init`.
2. **Token Discipline:** NEVER run `flagsmith flag list --json` without a `--jq` filter. Always extract only the required fields.
3. **Safety First:** Never delete or archive a flag in Flagsmith *before* the code removal PR is created and verified, unless explicitly instructed that the flag was never implemented in code.

---

## Step 1: Identify Candidate Stale Flags
Fetch only flag names, enabled states, and IDs in the target environment to keep output minimal:

```bash
flagsmith flag list --json --jq '[.[] | {id: .feature.id, name: .feature.name, enabled: .enabled, type: .feature.type}]'
```

* Identify flags that are candidates for retirement (e.g., flags the user named, or flags already enabled at 100% with no active experiment).
* If auditing broadly, inspect a specific candidate's full state and overrides using minimal fields:
  ```bash
  flagsmith flag get <feature_name> --json --jq '{name: .feature.name, enabled: .enabled, value: .feature_state_value, variants: .multivariate_feature_state_values}'
  ```

## Step 2: Audit Codebase References
Search the repository for every occurrence of `<feature_name>` (and camelCase/kebab-case variants if applicable):

```bash
rg -n "<feature_name>" --json | jq -r 'select(.type=="match") | "\(.data.path.text):\(.data.line_number): \(.data.lines.text | gsub("\n";""))"'
```
*(Fallback to `grep -rn "<feature_name>" .` if `rg` is unavailable.)*

Classify each match:
* **Zero matches:** Flag is orphaned in Flagsmith. Skip to Step 5 (confirm archival with user).
* **Active matches:** Inspect surrounding conditional blocks (`if (flagsmith.hasFeature(...))` or `getValue(...)`).

## Step 3: Remove Dead Code Paths
1. Create a dedicated branch:
   ```bash
   git checkout -b chore/remove-flag-<feature_name>
   ```
2. Refactor each call site based on the flag's settled state from Step 1:
   * **If settled `enabled: true`:** Keep the "enabled" branch, remove the `else`/fallback branch, and unwrap the conditional.
   * **If settled `enabled: false`:** Delete the "enabled" branch and keep the fallback behavior.
   * **If multivariate:** Inline the winning variant value.
3. Remove unused Flagsmith imports, mock fixtures in unit tests, and obsolete flag-specific tests.
4. Run the project's test suite and linter (check `package.json`, `Makefile`, or `pyproject.toml`) to verify zero regressions.

## Step 4: Open a Pull Request
Stage, commit, and open a PR using the GitHub CLI (`gh`):

```bash
git add -A
git commit -m "chore(flags): remove stale feature flag <feature_name>"
git push -u origin chore/remove-flag-<feature_name>
gh pr create \
  --title "chore(flags): retire <feature_name>" \
  --body "## Summary
- Removed stale Flagsmith flag \`<feature_name>\` (settled state: \`<enabled/value>\`).
- Cleaned up conditional branches and updated unit tests.

## Post-Merge Action
Archive/delete \`<feature_name>\` in Flagsmith once deployed to production."
```

## Step 5: Archive or Delete the Flag in Flagsmith
Ask the user whether they want to delete/archive `<feature_name>` in Flagsmith immediately (e.g., for non-production/orphaned flags) or wait until the PR merges. When confirmed, execute:

```bash
flagsmith flag delete <feature_name>
```
*(Or if archiving via API endpoint is preferred by the team's Flagsmith version, use `flagsmith api -X PATCH /api/v1/projects/<project_id>/features/<feature_id>/ -f is_archived=true --json --jq '{id, name, is_archived}'`.)*

## Output Summary Format
Conclude the run with a compact summary table:
* **Flag Retired:** `<feature_name>`
* **Files Modified:** `<count>`
* **PR Link:** `<url>`
* **Flagsmith Status:** `Deleted` / `Archived` / `Pending PR Merge`
