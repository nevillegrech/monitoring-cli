# install.ps1 — install dedaub-monitoring (Windows).
#
#   powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/Dedaub/monitoring-cli/main/install.ps1 | iex"
#
# It builds the CLI from the public repository with cargo, installs the agent
# skill, and starts the login. Running it again upgrades an existing install.
$ErrorActionPreference = 'Stop'

$Repo = 'https://github.com/Dedaub/monitoring-cli'

function Info($m) { Write-Host "==> $m" -ForegroundColor Blue }
function Warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
function Have($name) { [bool](Get-Command $name -ErrorAction SilentlyContinue) }

# cargo installs into %USERPROFILE%\.cargo\bin, which a fresh rustup has not put on PATH yet.
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

if (-not (Have 'cargo')) {
  throw "cargo is required. Install Rust from https://rustup.rs, open a new terminal, and run this installer again."
}

Info "Building dedaub-monitoring from $Repo (a few minutes the first time)…"
cargo install --locked --force --git $Repo
if ($LASTEXITCODE -ne 0) { throw "cargo failed to build the CLI (exit $LASTEXITCODE)." }

if (-not (Have 'dedaub-monitoring')) {
  Warn "dedaub-monitoring is installed but not on PATH in this session."
  Warn "Open a new terminal, then run: dedaub-monitoring install-skill ; dedaub-monitoring login"
  return
}

Info "Installing the agent skill…"
try { dedaub-monitoring install-skill } catch { Warn "Skill install skipped — run 'dedaub-monitoring install-skill' later." }

Info "Starting login (prints a URL to open in your browser)…"
try { dedaub-monitoring login } catch { Warn "Login skipped — run 'dedaub-monitoring login' when ready." }

Info "Done. Try:  dedaub-monitoring status ; dedaub-monitoring guide"
