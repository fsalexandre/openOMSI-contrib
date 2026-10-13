<#
  Configuração do servidor openOMSI no Docker (esta pasta), por menu:
    1) Trocar mapa   - manda o mapa escolhido (e os objetos, splines e carros que ele usa)
                       para o servidor, ajusta o server.cfg e reinicia
    2) Nome / mensagem
    3) Tráfego / jogadores
    4) Reiniciar
    5) Ver status

  Uso (PowerShell, nesta pasta):  .\configurar-servidor.ps1
  O mapa vai para a pasta content (montada no servidor). No mesmo disco do OMSI os arquivos
  entram como links físicos, sem ocupar espaço de novo. O servidor não entrega o mapa aos
  jogadores (share_maps = 0): cada um instala o mapa por conta própria.
#>
param([string]$Exe = "")
$ErrorActionPreference = "Stop"
# (Windows PowerShell 5.1 não tem $PSScriptRoot no param)
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$cfgPath = Join-Path $here "config\server.cfg"
$content = Join-Path $here "content"
$container = "openomsi-server"

function Get-OmsiDir {
    $envFile = Join-Path $here ".env"
    if (Test-Path $envFile) {
        foreach ($l in Get-Content $envFile) {
            if ($l -match '^\s*OMSI_DIR\s*=\s*(.+)$') { return $Matches[1].Trim().Trim('"') }
        }
    }
    throw "OMSI_DIR não está no .env desta pasta (copie .env.example para .env e ajuste)."
}

function Get-GameExe {
    if ($Exe) { return $Exe }
    $repo = Split-Path -Parent (Split-Path -Parent $here)
    $built = Join-Path $repo "target\release\openomsi.exe"
    if (Test-Path $built) { return $built }
    $dist = Get-ChildItem (Join-Path $repo "dist") -Filter "openomsi.exe" -Recurse -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($dist) { return $dist.FullName }
    throw "openomsi.exe não encontrado: passe -Exe <caminho do openomsi.exe>."
}

function Get-Cfg($key) {
    if (-not (Test-Path $cfgPath)) { return "" }
    foreach ($l in Get-Content $cfgPath) {
        if ($l -match "^\s*$key\s*=\s*(.*)$") { return $Matches[1].Trim() }
    }
    return ""
}

function Set-Cfg($key, $value) {
    $lines = @()
    if (Test-Path $cfgPath) { $lines = @(Get-Content $cfgPath) }
    $done = $false
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match "^\s*$key\s*=") { $lines[$i] = "$key = $value"; $done = $true }
    }
    if (-not $done) { $lines += "$key = $value" }
    New-Item -ItemType Directory -Force (Split-Path -Parent $cfgPath) | Out-Null
    # (sem BOM: o servidor lê o arquivo como texto simples)
    [IO.File]::WriteAllLines($cfgPath, [string[]]$lines, (New-Object Text.UTF8Encoding($false)))
}

function Get-WebPort {
    $p = Get-Cfg "web_port"
    if ($p) { return $p } else { return "27025" }
}

function Show-Status {
    try {
        $s = Invoke-RestMethod -Uri "http://127.0.0.1:$(Get-WebPort)/status" -TimeoutSec 5
        Write-Host ""
        Write-Host ("  Servidor : {0}" -f $s.name)
        Write-Host ("  Mapa     : {0}" -f $s.map)
        Write-Host ("  Jogadores: {0} de {1}" -f $s.players, $s.max_players)
        Write-Host ("  Hora     : {0}" -f $s.time)
    } catch {
        Write-Host "  O servidor não respondeu (parado, ou ainda carregando o mapa)." -ForegroundColor Yellow
    }
}

function Restart-Server {
    Write-Host "Reiniciando o servidor..."
    Push-Location $here
    try { cmd /c "docker compose up -d --force-recreate 2>&1" | Out-Null } finally { Pop-Location }
    $t0 = Get-Date
    while (((Get-Date) - $t0).TotalMinutes -lt 15) {
        try {
            Invoke-RestMethod -Uri "http://127.0.0.1:$(Get-WebPort)/status" -TimeoutSec 3 | Out-Null
            Write-Host ("Servidor no ar ({0:N0} s)." -f ((Get-Date) - $t0).TotalSeconds) -ForegroundColor Green
            return
        } catch { Write-Host -NoNewline "."; Start-Sleep -Seconds 3 }
    }
    Write-Host ""
    Write-Host "O servidor não respondeu em 15 minutos: veja 'docker compose logs' nesta pasta." -ForegroundColor Yellow
}

function Install-Map {
    $omsi = Get-OmsiDir
    $maps = @(Get-ChildItem (Join-Path $omsi "maps") -Directory | Where-Object { Test-Path (Join-Path $_.FullName "global.cfg") } | Sort-Object Name)
    if ($maps.Count -eq 0) { Write-Host "Nenhum mapa em $omsi\maps."; return }
    $current = Get-Cfg "map"
    Write-Host ""
    for ($i = 0; $i -lt $maps.Count; $i++) {
        $mark = if ($current -like "maps/$($maps[$i].Name)/*") { "  (atual)" } else { "" }
        Write-Host ("  {0,3}) {1}{2}" -f ($i + 1), $maps[$i].Name, $mark)
    }
    $n = Read-Host "Mapa (número, Enter volta)"
    if (-not $n) { return }
    $k = 0
    if (-not [int]::TryParse($n, [ref]$k) -or $k -lt 1 -or $k -gt $maps.Count) { Write-Host "Número inválido."; return }
    $name = $maps[$k - 1].Name
    $game = Get-GameExe
    $log = Join-Path $env:TEMP "openomsi-map-pack.log"
    Write-Host "Enviando '$name' para o servidor (o mapa e o que ele usa)..."
    $argv = @("--root", "`"$omsi`"", "--pack-map", "`"$name`"", "--pack-to", "`"$content`"")
    $p = Start-Process -FilePath $game -ArgumentList $argv -RedirectStandardError $log -RedirectStandardOutput "$log.out" -PassThru -WindowStyle Hidden
    $last = ""
    while (-not $p.HasExited) {
        Start-Sleep -Milliseconds 700
        $line = Get-Content $log -Tail 1 -ErrorAction SilentlyContinue
        if ($line -and $line -ne $last) { Write-Host "  $line"; $last = $line }
    }
    $out = Get-Content $log -ErrorAction SilentlyContinue
    $packed = $out | Select-String "^Packed " | Select-Object -Last 1
    if ($p.ExitCode -ne 0 -or -not $packed) {
        Write-Host "Não deu certo:" -ForegroundColor Red
        $out | Select-Object -Last 15 | ForEach-Object { Write-Host "  $_" }
        return
    }
    Write-Host $packed.Line -ForegroundColor Green
    $missing = $out | Select-String "files the map names are not on this machine"
    if ($missing) {
        Write-Host "Atenção: o mapa cita arquivos que não existem nesta máquina (nem o servidor nem os jogadores terão):" -ForegroundColor Yellow
        $out | Select-String "^  " | Select-Object -First 10 | ForEach-Object { Write-Host $_.Line }
        Write-Host "  (lista completa em $content\openomsi-map-pack-*.txt)"
    }
    Set-Cfg "map" "maps/$name/global.cfg"
    Restart-Server
    Show-Status
}

function Edit-Name {
    $n = Read-Host "Nome do servidor [$(Get-Cfg 'name')]"
    if ($n) { Set-Cfg "name" $n }
    $m = Read-Host "Mensagem de boas-vindas [$(Get-Cfg 'motd')]"
    if ($m) { Set-Cfg "motd" $m }
    if ($n -or $m) { Write-Host "Salvo. Vale depois de reiniciar (opção 4)." }
}

function Edit-Traffic {
    $t = Read-Host "Tráfego, carros de IA [$(Get-Cfg 'traffic')]"
    if ($t) { Set-Cfg "traffic" $t }
    $j = Read-Host "Máximo de jogadores [$(Get-Cfg 'max_players')]"
    if ($j) { Set-Cfg "max_players" $j }
    if ($t -or $j) { Write-Host "Salvo. Vale depois de reiniciar (opção 4)." }
}

while ($true) {
    Write-Host ""
    Write-Host "=== Servidor openOMSI ===" -ForegroundColor Cyan
    Write-Host "Mapa atual: $(Get-Cfg 'map')"
    Write-Host "1) Trocar mapa"
    Write-Host "2) Nome / mensagem"
    Write-Host "3) Tráfego / jogadores"
    Write-Host "4) Reiniciar"
    Write-Host "5) Ver status"
    Write-Host "0) Sair"
    switch (Read-Host "Escolha") {
        "1" { Install-Map }
        "2" { Edit-Name }
        "3" { Edit-Traffic }
        "4" { Restart-Server }
        "5" { Show-Status }
        "0" { return }
        default { }
    }
}
