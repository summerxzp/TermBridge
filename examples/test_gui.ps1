Add-Type -AssemblyName System.Windows.Forms
$result = [System.Windows.Forms.MessageBox]::Show('If you see this, GUI works. Click OK.', 'GUI Test', 'OKCancel')
Write-Host "MessageBox result: $result"
