---
paths:
  - "**/*.ps1"
  - "**/*.psm1"
---

# PowerShell Security Hardening (Windows PowerShell 5.1)

**Baseline:** Windows PowerShell 5.1 (Desktop edition). All guidance applies to 5.1; PS7+ notes
marked. This guide assumes scripts may run in security-sensitive environments (admin, automation,
credential handling).

## Execution Policy

**What it does:** Controls whether scripts can run. Does NOT prevent locally-written scripts (only
downloaded scripts marked with ZoneId).

**Do NOT use `-ExecutionPolicy Bypass` in production.** It disables the only protection for
downloaded scripts. If scripts are local or signed, set a narrower policy:

```powershell
# On target machine: set to RemoteSigned or AllSigned
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser -Force

# In automated contexts: sign scripts with a code-signing cert
Set-AuthenticodeSignature -FilePath "script.ps1" -Certificate $cert
```

**Risk:** `-ExecutionPolicy Bypass` is a common attack vector. Bypass only in non-persistent
contexts (CI, containers where the policy is reset on next boot).

## Credential Handling

**Never hardcode credentials.** Use one of:

```powershell
# Option 1: PSCredential (user provides at runtime)
$cred = Get-Credential -UserName "username"
Get-Process -Credential $cred

# Option 2: Encrypted credential object (persisted securely)
# Export once:
$cred = Get-Credential
$cred | Export-Clixml -Path "$PROFILE\..\cred.xml" -Force

# Use later (only user who encrypted can decrypt):
$cred = Import-Clixml -Path "$PROFILE\..\cred.xml"

# Option 3: SecureString for passwords (not ideal for long-term storage)
$plaintext = "MyPassword"
$secure = ConvertTo-SecureString -String $plaintext -AsPlainText -Force
$cred = New-Object System.Management.Automation.PSCredential -ArgumentList "user", $secure
```

**5.1 limitation:** No `SecureString` decryption without the original encryption context
(user + machine). For CI/CD, use a secrets management system (HashiCorp Vault, Azure KeyVault, etc.)
instead.

**Risk:** Plain-text passwords in scripts, logs, or history. Always review `$PSCommandPath` and
`Get-History` before sharing script output.

## Avoiding Injection Attacks

**Never use `Invoke-Expression` with user input.** It parses and executes arbitrary code:

```powershell
# DANGEROUS: user input + Invoke-Expression
$userInput = Read-Host "Enter a command"
Invoke-Expression $userInput  # Attacker can inject: "Get-Process; Remove-Item -Recurse /"

# Safe: structured parsing
$userInput = Read-Host "Enter a property name"
Get-Process | Select-Object -Property @($userInput)  # Only property names, no code execution
```

**Other risky patterns:**

- `& $variable` with user input (dynamic code execution)
- `-ScriptBlock { ... }` with string concatenation
- String interpolation in SQL-like queries

**Safe alternative:** Use parameterized APIs, allowlists, or structured parsing:

```powershell
# Good: parameterized via function params, not string building
$targetProp = "Name", "ID"  # Allowlist
Get-Process | Select-Object -Property $targetProp

# OK: Split-Path, Join-Path (safe path parsing)
$userPath = "../../etc/passwd"  # Attacker tries path traversal
$safe = Join-Path -Path $PSScriptRoot -ChildPath $userPath -Resolve  # Fails safely
```

## Script Signing

Sign scripts for distribution or sensitive environments:

```powershell
# Create a self-signed cert for testing (not production)
$cert = New-SelfSignedCertificate -DnsName "MyScripts" `
  -Type CodeSigning -CertStoreLocation Cert:\CurrentUser\My

# Sign a script
Set-AuthenticodeSignature -FilePath "script.ps1" -Certificate $cert

# Verify signature
Get-AuthenticodeSignature -FilePath "script.ps1"

# Set policy to require signatures
Set-ExecutionPolicy -ExecutionPolicy AllSigned -Scope CurrentUser
```

**5.1 note:** Self-signed certs are sufficient for local/internal use. For public distribution,
use a CA-signed code-signing certificate.

## Remote Execution (WinRM/Remoting)

**Enable WinRM securely:**

```powershell
# Enable only HTTPS (not HTTP)
Enable-PSRemoting -Force
$WsManCfg = @{
    Protocol = 'HTTPS'
    CertThumbprint = (Get-ChildItem Cert:\LocalMachine\My `
      | Select-Object -First 1).Thumbprint
}
Set-WSManInstance -ResourceURI winrm/config/Listener `
  -SelectorSet @{Address = '*'; Transport = 'HTTPS'} `
  -ValueSet $WsManCfg
```

**Use domain accounts and constrained endpoints when possible:**

```powershell
# Constrained endpoint: limit what commands can run
Register-PSSessionConfiguration -Name "LimitedCLI" `
  -ScriptBlock { ... } -RunAsCredential $svcAcct
```

**Risk:** WinRM over HTTP exposes credentials and data. Always use HTTPS +
certificate validation.

## Common Pitfalls

| Pitfall | Safe Pattern |
| --------- | ------------- |
| Hardcoded creds | Use `Get-Credential`, encrypted XML, vault |
| `Invoke-Expression` with user input | Parse input, use allowlists, avoid it |
| `-ExecutionPolicy Bypass` in scripts | Use signed scripts + RemoteSigned policy |
| Passing passwords as plain text | Use `SecureString`, encrypted storage |
| Running scripts as admin by default | Request elevation only when needed |
| No error handling on sensitive ops | Use `-ErrorAction Stop`, try/catch |
| Logging secrets | Filter logs before output |

## Auditing & Logging

Enable transcription and module logging for sensitive scripts:

```powershell
# Enable transcript (records all commands + output)
Start-Transcript -Path "$PROFILE\..\transcript.log"

# Module logging (log module import/invocation)
$moduleName = "MyModule"
$modPath = "HKLM:\Software\Policies\Microsoft\Windows\PowerShell\" +
  "ModuleLogging\ModuleNames"
New-Item -Path $modPath -Force
Set-ItemProperty -Path $modPath -Name $moduleName -Value "*"

# Script block logging (log all script blocks executed)
$blockPath = "HKLM:\Software\Policies\Microsoft\Windows\PowerShell\" +
  "ScriptBlockLogging"
New-Item -Path $blockPath -Force
Set-ItemProperty -Path $blockPath -Name "EnableScriptBlockLogging" -Value 1
```

## Related

- PowerShell Security Best Practices [MSDN][1]
- PSScriptAnalyzer Rules [GitHub][2]
- OWASP: Code Injection [OWASP][3]

[1]: https://docs.microsoft.com/en-us/powershell/scripting/learn/ps101/
[2]: https://github.com/PowerShell/PSScriptAnalyzer
[3]: https://owasp.org/www-community/attacks/Code_Injection
