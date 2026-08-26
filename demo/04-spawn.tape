Output "demo/04-spawn.mp4"
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
Type "a"
Sleep 2000ms
Screenshot "demo/04-spawn.png"
Down@600ms 2
Sleep 1500ms
Escape
Sleep 1200ms
