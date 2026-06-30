---
paths:
  - "**/*.ps1"
  - "**/*.psm1"
  - "**/*.psd1"
---

# PowerShell Conventions (Windows PowerShell 5.1)

**Baseline:** Windows PowerShell 5.1 (Desktop edition, built-in to Windows 10). All guidance
valid on 5.1 with no extra install. PS7+ notes marked explicitly; features unique to 7+ are
called out with 5.1-compatible alternatives.

## Naming & Casing

**PascalCase for:**
- Function names (e.g., `Get-ChildItem`, `Invoke-WebRequest`)
- Class names (e.g., `MyClass`)
- Enum names
- Module names

**camelCase for:**
- Parameter names (e.g., `-FilePath`, `-MaxRetries`)
- Variable names (e.g., `$userName`, `$maxRetries`)

**SCREAMING_SNAKE_CASE for:**
- Constants (rare; prefer `[Readonly]` on PSObjects instead)

## Function Design

```powershell
# Good: verb-noun naming, clear params, pipeline-friendly output
function Get-UserInfo {
    [CmdletBinding()]
    param(
        [Parameter(ValueFromPipeline = $true)]
        [string]$UserName,

        [int]$Retries = 3
    )
    process {
        # Implementation
    }
}

# Bad: unclear naming, positional params, non-pipelineable
function GetUser($u, $r) {
    # Implementation
}
```

**Required patterns:**
- `[CmdletBinding()]` attribute for any exported function (enables `-Verbose`,
  `-Debug`, etc.)
- `[Parameter()]` attributes for advanced binding (ValueFromPipeline,
  ValueFromPipelineByPropertyName)
- Return objects that can be piped (not `[void]` unless deliberate)
- Use `-ErrorAction Stop` for fail-fast behavior in scripts

## Approved Verbs (Cmdlet Design)

Use approved PowerShell verbs (part of the naming standard). Common ones:
`Get`, `Set`, `Add`, `Remove`, `Clear`, `Close`, `Find`, `Format`, `Invoke`,
`Join`, `New`, `Read`, `Rename`, `Split`, `Test`, `Write`.

Avoid non-standard verbs—they break discoverability and IDE hints.

## Error Handling

```powershell
# Preferred: use try/catch with typed exceptions
try {
    $result = Invoke-Command -ComputerName $target -ScriptBlock { whoami }
} catch [System.InvalidOperationException] {
    Write-Error "Failed to reach target: $_"
    exit 1
} catch {
    Write-Error "Unexpected error: $_"
    exit 2
}

# OK: -ErrorAction Stop to make non-terminating errors fatal
Get-Item -Path "C:\nonexistent" -ErrorAction Stop
```

**5.1 specifics:**
- No `try-catch` with pattern matching (PS7+ feature)—use `$_.GetType().Name` to branch
- No `&&` / `||` pipeline chaining (PS7+ feature)—use `if/else` or `; if ($?) { }`
- No `ForEach-Object -Parallel` (PS7+ only)—use sequential `foreach` or threadpool manually

## Scripting Best Practices

**Explicit over implicit:**
```powershell
# Good: parameter types, explicit property access
function Test-Path {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )
}

# Avoid: implicit types, positional shortcuts
function test-path($p) { ... }
```

**Prefer hash tables and custom PSObjects over magic strings:**
```powershell
# Good: structured output
$result = @{
    Success = $true
    Message = "Completed"
}
return [PSCustomObject]$result

# Avoid: magic strings or Write-Host output (not pipelineable)
Write-Host "Done"
```

**Loop idioms (5.1-compatible):**
```powershell
# Preferred: foreach statement (most readable, fastest)
foreach ($item in $collection) {
    # process $item
}

# OK: Where-Object + ForEach-Object (pipeline-friendly)
$collection | Where-Object { $_.Active } | ForEach-Object { ... }

# Avoid: @() syntax collecting arrays (inefficient)
$results = @()
foreach ($item in $collection) {
    $results += $item  # Bad: creates new array on each iteration
}
# Better: use [System.Collections.Generic.List[T]] or collect pipeline output
$results = [System.Collections.Generic.List[object]]::new()
$results.AddRange(@( $collection | Where-Object { $_ } ))
```

## Comments & Documentation

```powershell
<#
.SYNOPSIS
Brief description (one line)

.DESCRIPTION
Detailed description. What does it do? Why?

.PARAMETER ParamName
Description of ParamName. Include valid values if restricted.

.INPUTS
Type of objects accepted via pipeline (if any)

.OUTPUTS
Type of objects returned

.EXAMPLE
PS> Get-UserInfo -UserName "alice"
Retrieves user info for alice.

.NOTES
Any gotchas, prerequisites, or version notes

.LINK
https://docs.microsoft.com/en-us/powershell/...
#>
```

**Inline comments only for "why," not "what":**
```powershell
# Good: explains non-obvious decision
if ($retries -gt 3) {
    # Cap retries at 3 to avoid runaway loops in batch scenarios
}

# Bad: restates the code
$count = 0  # Initialize count
```

## Formatting & Style

- **Indentation:** 4 spaces (not tabs)
- **Braces:** opening on same line, closing on own line (Allman style)
- **Line length:** ≤100 chars where feasible
- **Splatting:** use `@{}` syntax for long parameter lists

```powershell
# Good: readable, splatted params
$params = @{
    ComputerName = $target
    Credential   = $cred
    Timeout      = 30
    ErrorAction  = 'Stop'
}
Get-Process @params

# Avoid: long one-liner
Get-Process -ComputerName $target -Credential $cred -Timeout 30 -ErrorAction Stop
```

## Performance Notes

- Prefer `[System.Collections.Generic.List[T]]` for large dynamic arrays (avoid `+=`)
- Use `-Filter` in `Get-ChildItem` (applies server-side); avoid `Where-Object` for large datasets
- Cache function results in variables; don't re-invoke in loops
- Avoid `Invoke-Expression` (security + performance risk)—parse structured input instead

## Testing

Use Pester v4 (built into Windows PowerShell 5.1+). Structure tests as `DescribeBlock`/`ItBlock`:

```powershell
Describe "Get-UserInfo" {
    It "returns user object when user exists" {
        $result = Get-UserInfo -UserName "alice"
        $result.Name | Should -Be "alice"
    }

    It "throws when user does not exist" {
        { Get-UserInfo -UserName "nonexistent" -ErrorAction Stop } | Should -Throw
    }
}
```

## Related

- PowerShell Development Guidelines [MSDN][1]
- PowerShell Practice and Style Guide [PoshCode][2]
- PSScriptAnalyzer Rules [GitHub][3]

[1]: https://docs.microsoft.com/en-us/powershell/scripting/developer/
[2]: https://github.com/PoshCode/PowerShellPracticeAndStyle
[3]: https://github.com/PowerShell/PSScriptAnalyzer/tree/master/Engine/Rules
