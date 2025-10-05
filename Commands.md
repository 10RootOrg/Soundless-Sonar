Build the program

cargo build --release

Run The Program

cargo run --release

mozart
target/release/sonar-presence.exe --mode scan --scan-url "https://www.youtube.com/watch?v=k1-TrAvp_xs"

blues
target/release/sonar-presence.exe --mode scan --scan-url "https://www.youtube.com/watch?v=9Wz2uAx_Bc4"

rap
target/release/sonar-presence.exe --mode scan --scan-url "https://www.youtube.com/watch?v=lg3DB0IziII"

video (skit or movie etc..)
target/release/sonar-presence.exe --mode scan --scan-url "https://www.youtube.com/watch?v=JYqfVE-fykk"

run offline
target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\20 - 19. The Bigger They Are.m4b"
target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\04 - Chapter 3.m4b"
target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\Fuck You - Lily Allen (Lyrics).mp3"
target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\Pompeii lyrics- Bastille.mp3"
target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\'I'm Eating Your Pie' - Above All That Is Random 2 - Christina Grimmie & Sarah.mp3"
target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\Fuck You - Lily Allen (Lyrics)\_3pings.flac"

target/release/sonar-presence.exe --mode offline --input "D:\Development\Sonar-Web\SonarVid\Pompeii lyrics- Bastille_3pings.flac"

gated mode
target/release/sonar-presence.exe --mode gated --sample-rate 44100

enrich mode

target/release/sonar-presence.exe --mode enrich --song-path "D:\Development\Sonar-Web\SonarVid\Pompeii lyrics- Bastille.mp3"

FFMPEG
SINGLE TONE over the entire vedio
.\ffmpeg\bin\ffmpeg.exe -i "D:\Development\Sonar-Web\SonarVid\Fuck You - Lily Allen (Lyrics).mp3" -filter_complex "[0:a]aresample=48000, aformat=sample_rates=48000:channel_layouts=stereo[a]; aevalsrc=exprs='0.01*sin(2*PI*19000*t)':s=48000:d=99699:channel_layout=stereo[u]; [a][u]amix=inputs=2:duration=first:dropout_transition=0[out]" -map "[out]" -c:a flac -map_metadata 0 "Fuck You - Lily Allen (Lyrics)\_ultra.flac"

a strong beep at the start and them single tone
.\ffmpeg\bin\ffmpeg.exe -hide_banner -i "D:\Development\Sonar-Web\SonarVid\Fuck You - Lily Allen (Lyrics).mp3" -filter_complex "[0:a]aresample=48000,aformat=sample_rates=48000:channel_layouts=stereo[a];aevalsrc=exprs='if(lt(t,5),0.25*sin(2*PI*1000*t),0.01*sin(2*PI*19000*t))':s=48000:d=999999:channel_layout=stereo[u];[a][u]amix=inputs=2:duration=first:dropout_transition=0[out]" -map "[out]" -c:a flac -map_metadata 0 "input_ultra_audiblestart.flac"

<!-- multiping  aevalsrc=exprs='(lt(mod(t,interval of ping),length of ping)' all in seconds -->

.\ffmpeg\bin\ffmpeg.exe -hide_banner -i "D:\Development\Sonar-Web\SonarVid\Pompeii lyrics- Bastille.mp3" -filter_complex "[0:a]aresample=48000,aformat=sample_rates=48000:channel_layouts=stereo[a];aevalsrc=exprs='(lt(mod(t,1),0.1))*pow(10,-35/20)*sin(2*PI*18500\*t)':s=48000:d=999999:channel_layout=stereo[u];[a][u]amix=inputs=2:duration=first:dropout_transition=0[out]" -map "[out]" -c:a flac -map_metadata 0 "D:\Development\Sonar-Web\SonarVid\Pompeii lyrics- Bastille_3pings.flac"
