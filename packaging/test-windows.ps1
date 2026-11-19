$ErrorActionPreference = 'Stop'
$key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey(
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', $true)
if (-not $key) { throw 'Cannot open system environment registry key' }
$options = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
$original = $key.GetValue('Path', $null, $options)
$kind = $key.GetValueKind('Path')
$installer = (Resolve-Path "dist\dowai-$env:VERSION-windows-x64-setup.exe").Path
$app = Join-Path $env:ProgramFiles 'DOWAI'
$uninstaller = Join-Path $app 'unins000.exe'

function Run-Installer($Path) {
    $process = Start-Process -FilePath $Path -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "$Path failed with exit code $($process.ExitCode)" }
}

try {
    # Exercise a PATH with a trailing separator and ensure upgrades retain ownership.
    $base = $original.TrimEnd(';') + ';'
    $key.SetValue('Path', $base, $kind)
    Run-Installer $installer
    if (($key.GetValue('Path', $null, $options)) -ne ($base + $app)) { throw 'PATH was not extended correctly' }
    if ((& (Join-Path $app 'dowai.exe') --version) -ne "dowai $env:VERSION") { throw 'Installed binary has the wrong version' }
    Run-Installer $installer
    if (($key.GetValue('Path', $null, $options)) -ne ($base + $app)) { throw 'Upgrade duplicated PATH' }
    Run-Installer $uninstaller
    if (($key.GetValue('Path', $null, $options)) -ne $base) { throw 'Uninstall did not restore PATH' }

    $base = $original.TrimEnd(';')
    $key.SetValue('Path', $base, $kind)
    Run-Installer $installer
    if (($key.GetValue('Path', $null, $options)) -ne ($base + ';' + $app)) { throw 'PATH separator was not added' }
    Run-Installer $uninstaller
    if (($key.GetValue('Path', $null, $options)) -ne $base) { throw 'Uninstall did not remove the added separator' }

    $existing = $original.TrimEnd(';') + ';"%ProgramFiles%\DOWAI"'
    $key.SetValue('Path', $existing, $kind)
    Run-Installer $installer
    if (($key.GetValue('Path', $null, $options)) -ne $existing) { throw 'Existing PATH entry was duplicated' }
    Run-Installer $uninstaller
    if (($key.GetValue('Path', $null, $options)) -ne $existing) { throw 'Pre-existing PATH entry was removed' }
} finally {
    $key.SetValue('Path', $original, $kind)
    $key.Close()
}
