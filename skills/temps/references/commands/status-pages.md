<!-- Generated from skills/temps-cli/references/COMMANDS.md. Do not edit manually. -->

# `status-pages` command reference

Apply [the CLI runtime and safety contract](../cli-runtime.md) before executing a command. Runtime `--help` is authoritative.

## `status-pages` (alias: `status-page`)

Publish public status pages for selected monitors

**Subcommands:**

- `list` (`ls`) - List a project's status pages
- `create` - Create a draft status page
- `show` - Show a status page
- `update` - Change the title, description or monitor selection
- `publish` - Publish the page at its public URL
- `unpublish` - Unpublish; the public URL returns 404 immediately
- `rotate-url` - Replace the public URL; the old one stops working
- `delete` (`rm`) - Delete a status page; its public URL returns 404 immediately

### `status-pages list` (alias: `ls`)

List a project's status pages

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--project-id <id>` | Project ID | - | Yes |
| `--json` | Output in JSON format | - | No |

### `status-pages create`

Create a draft status page

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--project-id <id>` | Project ID | - | Yes |
| `-t, --title <title>` | Public page title | - | Yes |
| `-d, --description <text>` | Public description | - | No |
| `-m, --monitor <id=name>` | Monitor to show and its public name (repeatable) | - | No |
| `--json` | Output in JSON format | - | No |

### `status-pages show`

Show a status page

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--id <id>` | Status page ID | - | Yes |
| `--json` | Output in JSON format | - | No |

### `status-pages update`

Change the title, description or monitor selection

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--id <id>` | Status page ID | - | Yes |
| `-t, --title <title>` | Public page title | - | No |
| `-d, --description <text>` | Public description ("" clears it) | - | No |
| `-m, --monitor <id=name>` | Replace the selection; repeat for each monitor | - | No |
| `--clear-monitors` | Remove every monitor (drafts only) | - | No |
| `--json` | Output in JSON format | - | No |

### `status-pages publish`

Publish the page at its public URL

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--id <id>` | Status page ID | - | Yes |
| `--json` | Output in JSON format | - | No |

### `status-pages unpublish`

Unpublish; the public URL returns 404 immediately

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--id <id>` | Status page ID | - | Yes |
| `--json` | Output in JSON format | - | No |

### `status-pages rotate-url`

Replace the public URL; the old one stops working

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--id <id>` | Status page ID | - | Yes |
| `--json` | Output in JSON format | - | No |

### `status-pages delete` (alias: `rm`)

Delete a status page; its public URL returns 404 immediately

**Options:**

| Flag | Description | Default | Required |
|------|-------------|---------|----------|
| `--id <id>` | Status page ID | - | Yes |
| `-y, --yes` | Skip the confirmation prompt | - | No |
