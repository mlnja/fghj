# Clip 3: the same https URL answers on the host and inside a container. ~8 s.
# Run after the files-web/full flow is up:  vhs 03-same-url.tape  -> 03-same-url.mp4
Output 03-same-url.mp4

Require fghj
Require curl
Set Shell "zsh"
Set FontSize 20
Set Width 1200
Set Height 720
Set Padding 28
Set WindowBar Colorful
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms

# exec zsh -f: a clean shell, so your own prompt theme stays out of the shot.
Hide
Type "exec zsh -f"
Enter
Sleep 500ms
Type "cd ~/files && setopt interactivecomments && PS1='%F{green}$%f ' && clear"
Enter
Sleep 500ms
Show

Sleep 300ms
Type "URL=https://web.files-web.files.fghj.internal/api/health"
Enter
Sleep 500ms

Type "# from the host"
Enter
Type "curl -s $URL"
Sleep 300ms
Enter
Sleep 1.5s

Type "# from inside the api container, same URL, same certificate"
Enter
Type `fghj exec api.files-api -- node -e "fetch('$URL').then(r=>r.text()).then(console.log)"`
Sleep 300ms
Enter
Sleep 3.5s
