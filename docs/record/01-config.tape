# Clip 1: the whole config of one repo. ~5 s.
# Run from anywhere:  vhs 01-config.tape   -> 01-config.mp4
Output 01-config.mp4

Require fghj
Set Shell "zsh"
Set FontSize 20
Set Width 1200
Set Height 720
Set Padding 28
Set WindowBar Colorful
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

# Everything between Hide and Show runs but is not recorded.
# exec zsh -f: a clean shell, so your own prompt theme stays out of the shot.
Hide
Type "exec zsh -f"
Enter
Sleep 500ms
Type "cd ~/files && setopt interactivecomments && PS1='%F{green}$%f ' && clear"
Enter
Sleep 500ms
Show

Sleep 400ms
Type `grep -v '#' files-web/.fghj.yaml | cat -s`
Sleep 400ms
Enter
Sleep 4.5s
