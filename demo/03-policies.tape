Output "demo/03-policies.mp4"
Set Shell "bash"
Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30
Set LoopOffset 20%
Hide
Type "lazybox --demo"
Enter
Wait+Screen@45s /rate limiter/
Show
Sleep 1500ms
Type "g"
Sleep 1800ms
Down@550ms 6
Sleep 1200ms
Screenshot "demo/03-policies.png"
Sleep 1500ms
