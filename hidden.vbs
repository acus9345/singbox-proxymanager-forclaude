' runs keep-alive.cmd completely hidden (no console flash every minute)
' from the "singbox-apps-keepalive" scheduled task. Path-independent.
CreateObject("Wscript.Shell").Run """" & CreateObject("Scripting.FileSystemObject").GetParentFolderName(WScript.ScriptFullName) & "\keep-alive.cmd""", 0, False
