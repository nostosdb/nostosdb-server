$ErrorActionPreference = "Stop"
throw @"
Windows Service registration is not implemented in this source preview.
nostd.exe is currently a foreground console process and does not implement
the Windows Service Control Manager ServiceMain/control-handler contract.
Run 'nostd.exe serve --config PATH' in a foreground terminal. Do not register
the console executable with sc.exe; a reviewed service host and credential ACL
installer are required before this script can safely create a service.
"@
