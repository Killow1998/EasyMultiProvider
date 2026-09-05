param([Parameter(Mandatory = $true)][string]$PackagePath)
# Native smoke test for Windows hosts without Python. Uses synthetic installations
# only, and stops processes only when their exact executable is a test copy.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$package = (Resolve-Path -LiteralPath $PackagePath).Path
$tempParent = (Resolve-Path -LiteralPath $env:TEMP).Path
$root = Join-Path $tempParent ('emp-update-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$root = (Resolve-Path -LiteralPath $root).Path
if ((Split-Path -Parent $root) -ne $tempParent) { throw 'Unsafe smoke test directory' }
$version = (& $package --version).Trim() -replace '^EMP ', ''
$ownedBinaries = @()
try {
    foreach ($failStartup in @($false, $true)) {
        $case = Join-Path $root $(if ($failStartup) { 'rollback' } else { 'success' })
        $job = Join-Path $case '.emp-update-test'
        $codexHome = Join-Path $case 'codex-home'
        New-Item -ItemType Directory -Path $job,$codexHome | Out-Null
        $target = Join-Path $case 'EMP.exe'
        $candidate = Join-Path $job 'candidate.exe'
        $helper = Join-Path $job 'worker.exe'
        $ownedBinaries += @($target, $helper)
        Copy-Item -LiteralPath $package -Destination $target
        Copy-Item -LiteralPath $package -Destination $candidate
        Copy-Item -LiteralPath $package -Destination $helper
        if ($failStartup) { [IO.File]::WriteAllText($candidate, 'invalid executable') }
        $listener = New-Object System.Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
        $listener.Start()
        $port = $listener.LocalEndpoint.Port
        $listener.Stop()
        $config = Join-Path $case 'config.json'
        @{ host = '127.0.0.1'; port = $port } | ConvertTo-Json | Set-Content -LiteralPath $config -Encoding ASCII
        $plan = Join-Path $job 'plan.json'
        @{
            target = $target; candidate = $candidate; relative_binary = ''
            parents = @(); args = @('serve','--config',$config,'--port',[string]$port)
            version = $version; nonce = 'windows-native-smoke'
        } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $plan -Encoding ASCII
        $env:CODEX_HOME = $codexHome
        $env:EASY_MULTI_PROVIDER_CONFIG = $config
        $env:EASY_MULTI_PROVIDER_MASTER_KEY = ''
        $env:EASY_MULTI_PROVIDER_MASTER_KEY_FILE = Join-Path $case 'master.key'
        $env:PYINSTALLER_RESET_ENVIRONMENT = '1'
        $worker = Start-Process -FilePath $helper -ArgumentList @('--emp-apply-update', ('"' + $plan + '"')) -WorkingDirectory $case -WindowStyle Hidden -PassThru
        if (-not $worker.WaitForExit(85000)) { throw 'Update worker timed out' }
        $expectedExit = if ($failStartup) { 1 } else { 0 }
        if ($worker.ExitCode -ne $expectedExit) { throw 'Unexpected update worker result' }
        $deadline = [DateTime]::UtcNow.AddSeconds(25)
        $healthy = $false
        while ([DateTime]::UtcNow -lt $deadline) {
            try {
                $health = Invoke-RestMethod -Uri ('http://127.0.0.1:' + $port + '/healthz') -TimeoutSec 2
                if ($health.status -eq 'ok') { $healthy = $true; break }
            } catch { }
            Start-Sleep -Milliseconds 200
        }
        if (-not $healthy) { throw 'Updated or restored service did not become healthy' }
        if ((Get-FileHash -LiteralPath $target).Hash -ne (Get-FileHash -LiteralPath $package).Hash) { throw 'Installed binary mismatch' }
        if ($failStartup -and -not (Test-Path -LiteralPath (Join-Path $job 'rolled-back'))) { throw 'Rollback was not confirmed' }
        if (-not $failStartup) {
            $deadline = [DateTime]::UtcNow.AddSeconds(15)
            while ((Test-Path -LiteralPath $job) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 200 }
            if (Test-Path -LiteralPath $job) { throw 'Successful update staging was not cleaned' }
        }
        [pscustomobject]@{ scenario = $(if ($failStartup) { 'rollback' } else { 'replacement' }); healthy = $healthy; passed = $true } | ConvertTo-Json -Compress
        Get-Process -Name EMP,worker -ErrorAction SilentlyContinue | Where-Object { $ownedBinaries -contains $_.Path } | Stop-Process -Force
    }
} catch {
    Get-ChildItem -LiteralPath $root -Recurse -Filter worker-status.json | ForEach-Object { Get-Content -LiteralPath $_.FullName }
    throw
} finally {
    $processes = @(Get-Process -Name EMP,worker -ErrorAction SilentlyContinue | Where-Object { $ownedBinaries -contains $_.Path })
    $processes | Stop-Process -Force -ErrorAction SilentlyContinue
    $processes | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    # Recheck containment immediately before recursive cleanup.
    $resolved = (Resolve-Path -LiteralPath $root).Path
    if ((Split-Path -Parent $resolved) -ne $tempParent -or (Split-Path -Leaf $resolved) -notlike 'emp-update-smoke-*') { throw 'Unsafe cleanup target' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
