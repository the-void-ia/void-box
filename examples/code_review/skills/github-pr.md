# GitHub Pull Request Workflow

Follow these steps to submit a Pull Request via the GitHub CLI.

## 1. Configure git identity

```bash
git config --global user.name "void-box[bot]"
git config --global user.email "void-box[bot]@users.noreply.github.com"
```

## 2. Set up GITHUB_TOKEN authentication

The `GITHUB_TOKEN` environment variable is already set. Configure git to use it for HTTPS pushes:

```bash
git config --global credential.helper store
printf 'protocol=https\nhost=github.com\nusername=x-access-token\npassword=%s\n' "$GITHUB_TOKEN" | git credential-store store
```

Also tell `gh` to use the token:

```bash
echo "$GITHUB_TOKEN" | gh auth login --with-token
```

## 3. Clone and branch

```bash
git clone https://github.com/<owner>/<repo>.git /workspace/repo
cd /workspace/repo
git checkout -b void-box/code-review-<short-description>
```

## 4. Apply changes and commit

Make the code changes, then:

```bash
git add -A
git commit -m "<concise summary of changes>"
```

## 5. Push and create PR

```bash
git push -u origin HEAD
gh pr create \
  --title "<PR title>" \
  --body "## Summary
<bullet list of changes>

---
*Automated by void-box code-review pipeline*"
```
