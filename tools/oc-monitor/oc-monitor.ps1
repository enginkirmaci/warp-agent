# oc-monitor.ps1 - opencode serve HTTP traffic monitor (Windows, no admin needed)
#
# A logging reverse proxy: clients connect to -MonitorPort, requests are logged
# (full headers + body preview) and forwarded to the real server on -RealPort.
# A live dashboard is served at http://127.0.0.1:<MonitorPort>/
#
#   opencode serve --hostname 127.0.0.1 --port 4096     (real server)
#   .\oc-monitor.ps1                                    (this proxy, port 5599)
#   point any client at 127.0.0.1:5599

param(
    [int]$RealPort = 4096,
    [int]$MonitorPort = 5599,
    [int]$Keep = 500
)

$ErrorActionPreference = 'Stop'

$dashboardHtml = @'
<!doctype html>
<html><head><meta charset="utf-8"><title>oc-monitor</title><style>
:root{color-scheme:dark}
*{box-sizing:border-box}
body{margin:0;background:#161616;color:#d4d4d4;font:13px/1.45 ui-monospace,'Cascadia Mono',Consolas,monospace}
header{display:flex;gap:12px;align-items:center;padding:9px 14px;background:#111;border-bottom:1px solid #2a2a2a;position:sticky;top:0;z-index:2}
header b{color:#4AC265}
#filter{flex:0 1 420px;background:#1d1d1d;border:1px solid #333;border-radius:6px;color:#d4d4d4;padding:5px 9px;outline:none}
#filter:focus{border-color:#4AC265}
label{display:flex;gap:5px;align-items:center;color:#888;cursor:pointer;user-select:none}
button{background:transparent;border:1px solid #333;border-radius:6px;color:#999;padding:4px 10px;cursor:pointer}
button:hover{color:#d4d4d4;border-color:#555}
.tag{color:#777;font-size:11px}
main{display:grid;grid-template-columns:1fr 400px}
#rows{overflow:auto;height:calc(100vh - 40px)}
table{width:100%;border-collapse:collapse}
th{position:sticky;top:0;background:#161616;text-align:left;color:#777;font-weight:500;padding:6px 10px;border-bottom:1px solid #2a2a2a}
td{padding:5px 10px;border-bottom:1px solid #212121;white-space:nowrap;cursor:pointer;max-width:460px;overflow:hidden;text-overflow:ellipsis}
tr:hover td{background:#1e1e1e}
tr.sel td{background:#223022}
.m{color:#7ec9ff}
.s2{color:#4AC265}.s4{color:#E8A348}.s5{color:#F47067}
.c{color:#666}
#detail{border-left:1px solid #2a2a2a;background:#141414;padding:12px 14px;overflow:auto;height:calc(100vh - 40px)}
#detail h3{margin:2px 0 8px;font-size:13px;color:#fff;word-break:break-all}
.k{color:#999}.v{color:#c9c9c9;word-break:break-all}
.flag .k{color:#E8A348;background:#3a2f18;border-radius:3px;padding:0 3px}
pre{background:#1b1b1b;border:1px solid #2a2a2a;border-radius:6px;padding:8px;white-space:pre-wrap;word-break:break-word;max-height:220px;overflow:auto;color:#bbb;margin:4px 0}
h4{margin:14px 0 4px;color:#888;font-weight:500;font-size:11px;text-transform:uppercase;letter-spacing:.4px}
.empty{color:#555;padding:24px;text-align:center}
</style></head><body>
<header><b>oc-monitor</b><span class="tag" id="target"></span>
<input id="filter" placeholder="filter... (method / path / status)">
<label><input type="checkbox" id="live" checked>live</label>
<button id="clear">clear</button><span class="tag" id="count"></span></header>
<main><div id="rows"><div class="empty">yukleniyor...</div></div>
<aside id="detail"><div class="empty">bir istek sec</div></aside></main>
<script>
var rows=[],sel=null;
var f=document.getElementById('filter');
function esc(s){return String(s==null?'':s).replace(/[&<>"']/g,function(c){return{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]})}
function flag(k){return/track|telemetr|analytic|sentry|fingerprint|device|client|machine|install|hardware/i.test(k)}
function sc(s){s=parseInt(s);return isNaN(s)?'c':s<300?'s2':s<500?'s4':'s5'}
async function poll(){
  if(!document.getElementById('live').checked)return;
  try{
    var r=await fetch('/_log',{cache:'no-store'});var d=await r.json();
    rows=d.entries||[];
    document.getElementById('target').textContent='\u2192 '+d.target;
    render();
  }catch(e){}
}
function render(){
  var q=f.value.toLowerCase();
  var list=rows.filter(function(e){return !q||(e.method+' '+e.path+' '+e.status).toLowerCase().indexOf(q)>=0});
  document.getElementById('count').textContent=list.length+' / '+rows.length;
  var h=list.length?'<table><thead><tr><th>time</th><th></th><th>path</th><th>status</th><th>ms</th><th>client</th></tr></thead><tbody>'
    +list.map(function(e){return '<tr data-id="'+e.id+'" onclick="pick('+e.id+')"><td class="c">'+esc(e.time)
      +'</td><td class="m">'+esc(e.method)+'</td><td class="p">'+esc(e.path)
      +'</td><td class="'+sc(e.status)+'">'+esc(e.status)+'</td><td class="c">'+e.ms
      +'</td><td class="c">'+esc(e.client)+(e.note?' <span class="tag">'+esc(e.note)+'</span>':'')+'</td></tr>'}).join('')
    +'</tbody></table>':'<div class="empty">trafik yok - client\'i proxy portuna yonelt</div>';
  document.getElementById('rows').innerHTML=h;
  if(sel!=null)markSel();
}
function pick(id){sel=id;markSel();show()}
function markSel(){var t=document.querySelectorAll('tr');for(var i=0;i<t.length;i++)t[i].classList.toggle('sel',+t[i].dataset.id===sel)}
function show(){
  var e=null;for(var i=0;i<rows.length;i++)if(rows[i].id===sel)e=rows[i];
  if(!e)return;
  function hl(a){if(!a||!a.length)return'<span class="tag">-</span>';
    return a.map(function(x){var i=x.indexOf(':');var k=x.slice(0,i),v=x.slice(i+1).trim();
      return'<div class="'+(flag(k)?'flag':'')+'"><span class="k">'+esc(k)+':</span> <span class="v">'+esc(v)+'</span></div>'}).join('')}
  document.getElementById('detail').innerHTML='<h3><span class="m">'+esc(e.method)+'</span> '+esc(e.path)+'</h3>'
    +'<div class="tag">'+esc(e.time)+' \u00b7 '+esc(e.client)+' \u00b7 '+esc(e.status)+' \u00b7 '+e.ms+'ms'+(e.note?' \u00b7 '+esc(e.note):'')+'</div>'
    +'<h4>request headers</h4>'+hl(e.reqHeaders)
    +(e.reqBody?'<h4>request body</h4><pre>'+esc(e.reqBody)+'</pre>':'')
    +'<h4>response headers</h4>'+hl(e.respHeaders)
    +(e.respBody?'<h4>response body</h4><pre>'+esc(e.respBody)+'</pre>':'');
}
document.getElementById('clear').onclick=async function(){await fetch('/_clear');sel=null;show();poll()};
f.oninput=render;
poll();setInterval(poll,1200);
</script>
</body></html>
'@

$sync = [hashtable]::Synchronized(@{
    Log    = New-Object System.Collections.ArrayList
    Lock   = New-Object object
    NextId = 1
})

$handler = {
    param($client, $realPort, $sync, $keep, $html)

    $enc  = [System.Text.Encoding]::ASCII
    $utf8 = [System.Text.Encoding]::UTF8

    function Add-Entry([object]$e) {
        [void][System.Threading.Monitor]::Enter($sync.Lock)
        try {
            $e.id = $sync.NextId; $sync.NextId++
            [void]$sync.Log.Add($e)
            while ($sync.Log.Count -gt $keep) { $sync.Log.RemoveAt(0) }
        } finally { [void][System.Threading.Monitor]::Exit($sync.Lock) }
    }

    # $chunkSize = 16384 for request heads (may overshoot into the body -> returned in Rest);
    # use 1 for response heads so Rest is always empty.
    function Read-Head($s, [int]$max, [int]$chunkSize) {
        $ms = New-Object System.IO.MemoryStream
        $buf = New-Object byte[] $chunkSize
        while ($true) {
            if ($ms.Length -gt $max) { throw 'http head too large' }
            $n = $s.Read($buf, 0, $chunkSize)
            if ($n -le 0) {
                if ($ms.Length -eq 0) { return $null }
                break
            }
            [void]$ms.Write($buf, 0, $n)
            $txt = $enc.GetString($ms.ToArray())
            $i = $txt.IndexOf("`r`n`r`n")
            if ($i -ge 0) {
                $all = $ms.ToArray()
                $headLen = $i + 4
                $head = New-Object byte[] $headLen
                [Array]::Copy($all, $head, $headLen)
                $restLen = $all.Length - $headLen
                $rest = New-Object byte[] $restLen
                [Array]::Copy($all, $headLen, $rest, 0, $restLen)
                return @{ Bytes = $head; Rest = $rest }
            }
        }
        return @{ Bytes = $ms.ToArray(); Rest = (New-Object byte[] 0) }
    }

    function Parse-Head([byte[]]$bytes) {
        $txt = $enc.GetString($bytes)
        $lines = $txt -split "`r`n"
        $h = @{ First = $lines[0]; Headers = @{} }
        for ($j = 1; $j -lt $lines.Count; $j++) {
            $c = $lines[$j].IndexOf(':')
            if ($c -gt 0) { $h.Headers[$lines[$j].Substring(0, $c).Trim().ToLower()] = $lines[$j].Substring($c + 1).Trim() }
        }
        return $h
    }

    function Read-Line($s) {
        $ms = New-Object System.IO.MemoryStream
        $b = New-Object byte[] 1
        while ($true) {
            $n = $s.Read($b, 0, 1)
            if ($n -le 0) { if ($ms.Length -eq 0) { return $null } else { break } }
            if ($b[0] -eq 10) { break }
            [void]$ms.WriteByte($b[0])
        }
        return $enc.GetString($ms.ToArray()).TrimEnd("`r")
    }

    function Copy-Exact($from, $to, [long]$n, $cap) {
        $buf = New-Object byte[] 65536
        while ($n -gt 0) {
            $want = [int][Math]::Min([long]$buf.Length, $n)
            $got = $from.Read($buf, 0, $want)
            if ($got -le 0) { break }
            $to.Write($buf, 0, $got)
            if ($cap -ne $null -and $cap.Length -lt 262144) { [void]$cap.Write($buf, 0, $got) }
            $n -= $got
        }
    }

    function Relay-Chunks($from, $to) {
        $cap = New-Object System.IO.MemoryStream
        while ($true) {
            $line = Read-Line $from
            if ($line -eq $null) { break }
            $lb = $enc.GetBytes($line + "`r`n"); $to.Write($lb, 0, $lb.Length)
            $hex = ($line -split ';')[0].Trim()
            $size = 0
            if (-not [int]::TryParse($hex, [System.Globalization.NumberStyles]::HexNumber, $null, [ref]$size)) { break }
            if ($size -eq 0) {
                while ($true) {
                    $t = Read-Line $from
                    if ($t -eq $null) { return $cap }
                    $tb = $enc.GetBytes($t + "`r`n"); $to.Write($tb, 0, $tb.Length)
                    if ($t -eq '') { return $cap }
                }
            }
            Copy-Exact $from $to ([long]($size + 2)) $cap
        }
        return $cap
    }

    function Relay-UntilClose($from, $to) {
        $cap = New-Object System.IO.MemoryStream
        $buf = New-Object byte[] 65536
        while ($true) {
            $got = $from.Read($buf, 0, $buf.Length)
            if ($got -le 0) { break }
            $to.Write($buf, 0, $got)
            if ($cap.Length -lt 262144) { [void]$cap.Write($buf, 0, $got) }
        }
        return $cap
    }

    function Preview([byte[]]$b) {
        if ($b -eq $null -or $b.Length -eq 0) { return $null }
        $n = [Math]::Min($b.Length, 2000)
        $t = $utf8.GetString($b, 0, $n)
        if ($b.Length -gt 2000) { $t += " ... ($($b.Length) bytes)" }
        return $t
    }

    function Send-Local($s, [string]$ctype, [string]$bodyStr, [string]$code) {
        $b = $utf8.GetBytes($bodyStr)
        $head = "HTTP/1.1 $code`r`nContent-Type: $ctype`r`nContent-Length: $($b.Length)`r`nCache-Control: no-store`r`nConnection: close`r`n`r`n"
        $hb = $utf8.GetBytes($head)
        $s.Write($hb, 0, $hb.Length)
        $s.Write($b, 0, $b.Length)
    }

    function Serve-Local($s, $method, $target) {
        if ($target -eq '/_clear') {
            [void][System.Threading.Monitor]::Enter($sync.Lock)
            try { $sync.Log.Clear() } finally { [void][System.Threading.Monitor]::Exit($sync.Lock) }
            Send-Local $s 'text/plain' 'ok' '200 OK'
            return
        }
        if ($target -eq '/_log') {
            [void][System.Threading.Monitor]::Enter($sync.Lock)
            try { $items = $sync.Log.ToArray() } finally { [void][System.Threading.Monitor]::Exit($sync.Lock) }
            if ($items.Count -eq 0) { $entriesJson = '[]' } else { $entriesJson = ConvertTo-Json -InputObject $items -Depth 5 -Compress }
            $payload = '{"target":"' + ("127.0.0.1:" + $realPort) + '","entries":' + $entriesJson + '}'
            Send-Local $s 'application/json' $payload '200 OK'
            return
        }
        Send-Local $s 'text/html; charset=utf-8' $html '200 OK'
    }

    try {
        $client.NoDelay = $true
        $cs = $client.GetStream()
        $cs.ReadTimeout = 600000

        while ($true) {
            $rh = Read-Head $cs 1048576 16384
            if ($rh -eq $null) { break }
            $req = Parse-Head $rh.Bytes
            $parts = $req.First -split ' ', 3
            if ($parts.Count -lt 2) { break }
            $method = $parts[0]; $target = $parts[1]

            # local dashboard endpoints
            if ($target -eq '/' -or $target.StartsWith('/_')) {
                Serve-Local $cs $method $target
                break
            }

            $e = [ordered]@{ id=0; time=(Get-Date -Format 'HH:mm:ss.fff'); client=(':'+($client.Client.RemoteEndPoint.Port)); method=$method; path=$target; status=''; ms=0; note=''; reqHeaders=@(); respHeaders=@(); reqBody=$null; respBody=$null }
            foreach ($h in $req.Headers.Keys) { $e.reqHeaders += ("{0}: {1}" -f $h, $req.Headers[$h]) }

            $sw = [System.Diagnostics.Stopwatch]::StartNew()
            $srv = New-Object System.Net.Sockets.TcpClient
            try {
                $srv.NoDelay = $true
                $srv.Connect('127.0.0.1', $realPort)
            } catch {
                $e.status = '502'; $e.note = 'server unreachable'; $e.ms = $sw.ElapsedMilliseconds
                Add-Entry $e
                Send-Local $cs 'text/plain' "opencode server unreachable on 127.0.0.1:$realPort" '502 Bad Gateway'
                break
            }
            $ss = $srv.GetStream()
            $ss.ReadTimeout = 3600000

            try {
                # forward the request head, then the body:
                #   content-length -> buffered for a log preview
                #   chunked        -> verbatim streaming copy (no preview)
                $ss.Write($rh.Bytes, 0, $rh.Bytes.Length)
                if ($req.Headers['content-length']) {
                    $need = [long]$req.Headers['content-length']
                    $ms = New-Object System.IO.MemoryStream
                    if ($rh.Rest.Length -gt 0) {
                        $take = [int][Math]::Min([long]$rh.Rest.Length, $need)
                        [void]$ms.Write($rh.Rest, 0, $take)
                        if ($rh.Rest.Length -gt $need) { $e.note = 'pipelined data dropped' }
                    }
                    if ($ms.Length -lt $need) { Copy-Exact $cs $ms ($need - $ms.Length) $null }
                    $body = $ms.ToArray()
                    if ($body.Length -gt 0) { $e.reqBody = Preview $body; $ss.Write($body, 0, $body.Length) }
                } elseif ($req.Headers['transfer-encoding'] -match 'chunked') {
                    $e.note = 'chunked request body'
                    if ($rh.Rest.Length -gt 0) { $ss.Write($rh.Rest, 0, $rh.Rest.Length) }
                    $null = Relay-Chunks $cs $ss
                }

                # response head (skip interim 100-continue)
                while ($true) {
                    $rph = Read-Head $ss 1048576 1
                    if ($rph -eq $null) { $e.status = 'EOF'; break }
                    $resp = Parse-Head $rph.Bytes
                    if ($resp.First -match '^HTTP/1\.\d 100') {
                        $cb = $utf8.GetString($rph.Bytes); $bb = $utf8.GetBytes($cb); $cs.Write($bb, 0, $bb.Length)
                        continue
                    }
                    break
                }
                if ($e.status -eq 'EOF') { Add-Entry $e; break }

                $e.status = ($resp.First -split ' ', 3)[1]
                $e.ms = $sw.ElapsedMilliseconds
                foreach ($h in $resp.Headers.Keys) { $e.respHeaders += ("{0}: {1}" -f $h, $resp.Headers[$h]) }
                if ($target -match 'event') { $e.note = 'event stream (stays open)' }
                elseif (-not $resp.Headers['content-length'] -and $resp.Headers['transfer-encoding'] -notmatch 'chunked') { $e.note = 'until close' }
                Add-Entry $e

                $cs.Write($rph.Bytes, 0, $rph.Bytes.Length)

                $statusNum = 0
                [void][int]::TryParse($e.status, [ref]$statusNum)
                $noBody = ($method -eq 'HEAD') -or ($statusNum -eq 204) -or ($statusNum -eq 304)
                $respClose = ($resp.Headers['connection'] -match 'close')

                if (-not $noBody) {
                    if ($resp.Headers['content-length']) {
                        $cap = New-Object System.IO.MemoryStream
                        Copy-Exact $ss $cs ([long]$resp.Headers['content-length']) $cap
                        $e.respBody = Preview $cap.ToArray()
                    } elseif ($resp.Headers['transfer-encoding'] -match 'chunked') {
                        $cap = Relay-Chunks $ss $cs
                        $e.respBody = Preview $cap.ToArray()
                    } else {
                        $null = Relay-UntilClose $ss $cs
                        break
                    }
                }

                $col = 'Gray'
                if ($statusNum -ge 500) { $col = 'Red' } elseif ($statusNum -ge 400) { $col = 'Yellow' }
                Write-Host ("  #{0} {1} {2} -> {3} ({4}ms){5}" -f $e.id, $e.method, $e.path, $e.status, $e.ms, $(if ($e.note) { '  [' + $e.note + ']' })) -ForegroundColor $col

                if ($respClose) { break }
                # keep-alive: loop for the next request on this connection
            } finally {
                try { $srv.Close() } catch {}
            }
        }
    } catch {
        # client disconnect / idle keep-alive timeout - normal end of connection
    } finally {
        try { $client.Close() } catch {}
    }
}

$listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, $MonitorPort)
$listener.Start()

Write-Host "[oc-monitor] proxy   : 127.0.0.1:$MonitorPort  ->  127.0.0.1:$RealPort"
Write-Host "[oc-monitor] dashboard: http://127.0.0.1:$MonitorPort/"
Write-Host "[oc-monitor] Ctrl+C to stop"

try {
    $probe = New-Object System.Net.Sockets.TcpClient
    $probe.Connect('127.0.0.1', $RealPort); $probe.Close()
    Write-Host "[oc-monitor] real server reachable on port $RealPort" -ForegroundColor Green
} catch {
    Write-Host "[oc-monitor] WARNING: nothing listening on $RealPort yet. Start it first:" -ForegroundColor Yellow
    Write-Host "[oc-monitor]   opencode serve --hostname 127.0.0.1 --port $RealPort" -ForegroundColor Yellow
}

$pool = New-Object System.Collections.ArrayList

while ($true) {
    $client = $listener.AcceptTcpClient()

    for ($i = $pool.Count - 1; $i -ge 0; $i--) {
        if ($pool[$i].Result.IsCompleted) { $pool[$i].PS.Dispose(); $pool.RemoveAt($i) }
    }

    $ps = [powershell]::Create()
    [void]$ps.AddScript($handler.ToString()).AddArgument($client).AddArgument($RealPort).AddArgument($sync).AddArgument($Keep).AddArgument($dashboardHtml)
    $res = $ps.BeginInvoke()
    [void]$pool.Add(@{ PS = $ps; Result = $res })
}
