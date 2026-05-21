# VTCast

VTuber 아바타(VTubeStudio / VSeeFace / Warudo)를 **투명 배경 그대로** OBS의 Browser Source로 송출할 수 있는 자체 호스팅 가능한 P2P WebRTC 도구입니다. OBS 플러그인이 필요 없고, 역할별로 단일 Rust 바이너리 하나씩으로 동작합니다.

기본 릴레이는 `https://vtcast.jamku.me`로 설정돼 있어 별도 서버 없이도 바로 사용 가능하지만, `vtcast-relay`를 직접 호스팅하면 모든 트래픽을 자신의 인프라 안에서 운영할 수 있습니다.

## 구성 요소

- **`vtcast-relay`** — 시그널링 브로커 + 내장 TURN을 한 프로세스에서 처리하는 서버. WebRTC 핸드셰이크 메시지를 라우팅하고 ICE 서버 정보를 발급합니다. 대칭 NAT 환경에서 TURN으로 fallback되는 경우를 제외하면 미디어는 이 서버를 거치지 않습니다. 수신용 웹페이지(`/r`)도 같이 서빙합니다.
- **`vtcast`** (GUI) — Windows 데스크톱 송신 앱. Spout / Windows Graphics Capture / Desktop Duplication API로 캡처 → 알파를 side-by-side로 패킹 → ffmpeg(libx264 / NVENC / QSV / AMF) 또는 Media Foundation으로 인코딩 → 구독자 1명당 WebRTC 트랙 1개로 송신.
- **`vtcast-cli`** — GUI와 같은 기능의 헤드리스 CLI. 자동화 / 서버 환경 / 디버깅용.
- **`vtcast-capture`** — Spout 수신 + WGC + DDA를 다루는 순수 Rust 캡처 라이브러리. GUI와 CLI가 공유합니다.

## 알파 채널 전송 원리

H.264는 알파 채널을 직접 지원하지 않습니다. VTCast는 **side-by-side 알파 패킹**으로 우회합니다:

- 인코더가 보는 프레임은 원본의 두 배 너비
- 왼쪽 절반: RGB
- 오른쪽 절반: 그레이스케일 알파

수신측 WebGL 셰이더가 그릴 때 RGBA를 다시 합성합니다. 즉 와이어로는 평범한 H.264가 흐르고, OBS Browser Source는 별도 플러그인 없이 바로 투명 배경으로 렌더링됩니다.

## 아키텍처

이전 버전에는 릴레이가 SFU로 미디어를 포워딩했지만, 현재는 **순수 mesh P2P**입니다. 송신자가 구독자 1명마다 별도의 `RTCPeerConnection`을 맺고, 인코딩된 H.264 샘플은 단일 `TrackLocalStaticSample`을 공유해 webrtc-rs가 각 PC로 fan-out합니다.

- 송신자 업로드는 구독자 수에 비례 (N × bitrate)
- 릴레이 대역폭은 거의 0 (시그널링은 분당 KB 단위)
- 가정용 100 Mbps 회선으로 8 Mbps × 약 10명 구독자 처리 가능
- VTuber 협업: 각 VTuber가 자신의 룸을 운영하고, 다른 VTuber는 그 룸의 수신 URL을 OBS Browser Source로 추가 → 릴레이는 룸 단위로 선형 확장

## 빠른 시작 (로컬 개발)

필요한 것:

- Windows 10/11 (송신측). 릴레이는 Linux/Windows 모두 OK.
- Rust 툴체인 (소스 빌드용)
- ffmpeg 백엔드를 쓸 경우 `ffmpeg.exe`가 PATH에 있어야 함
- 투명 출력이 가능한 VTuber 앱 (Warudo: Settings → Output → Spout Output 체크, Camera → Transparent Background)

릴레이 실행:

```powershell
cargo run -p vtcast-relay
```

다른 터미널에서 GUI 또는 CLI:

```powershell
cargo run -p vtcast-sender-app
# 또는
cargo run -p vtcast-sender -- --encoder nvenc
```

송신자가 다음과 같은 줄을 출력합니다:

```
OBS receiver URL: https://vtcast.jamku.me/r?room=fyryho
```

(로컬 릴레이를 사용 중이면 `http://localhost:17239/r?room=...`)

해당 URL을 **OBS Browser Source**에 붙여넣으면 끝. "Shutdown source when not visible"은 꺼두세요. 일반 브라우저에서 테스트할 때는 URL 뒤에 `?debug`를 붙이면 체커보드 배경과 상태 오버레이가 보입니다.

## 빌드 (릴리즈)

세 바이너리 모두:

```powershell
cargo build --release
```

릴레이만:

```bash
cargo build --release -p vtcast-relay --bin vtcast-relay
```

산출물:

- `target/release/vtcast.exe` — 송신 GUI (Tauri 2)
- `target/release/vtcast-cli.exe` — 송신 CLI
- `target/release/vtcast-relay.exe` — 릴레이 (Linux 빌드 시 `.exe` 없음)

배포용 NSIS 인스톨러는 `cargo tauri build -c crates/sender-app/tauri.conf.json`으로 생성. 산출물은 `target/release/bundle/nsis/`.

## CLI 옵션 (vtcast-cli)

```
vtcast-cli [--relay URL] [--room CODE] [--source spout|window|display]
           [--sender NAME] [--window-title NAME]
           [--fps 30] [--bitrate 8000]
           [--encoder libx264|nvenc|qsv|amf] [--backend ffmpeg|mf]
           [--chroma-key R,G,B,THRESHOLD,SOFTNESS]
```

| 옵션 | 기본값 | 설명 |
| --- | --- | --- |
| `--relay` | `https://vtcast.jamku.me` | 시그널링 릴레이 베이스 URL. 자체 호스팅 릴레이를 쓰면 여기를 바꿈. |
| `--room` | 자동 발급 | 룸 코드 지정. 생략 시 릴레이가 새 코드 발급. |
| `--source` | (필수) | `spout` / `window` / `display` 중 하나. |
| `--sender` | 첫 번째 송신 | Spout 송신 이름. 부분 일치. |
| `--window-title` | — | window 캡처 시 매칭할 창 제목, display 캡처 시 모니터 인덱스. |
| `--fps` | 30 | 인코더 목표 프레임레이트. |
| `--bitrate` | 8000 | 목표 비트레이트 (kbps). |
| `--encoder` | 자동 선택 | 인코더 명시 지정. NVENC가 권장. |
| `--backend` | `ffmpeg` | `ffmpeg`(외부 의존) 또는 `mf`(Windows Media Foundation 직접). |
| `--chroma-key` | 비활성화 | 키 컬러 R,G,B + threshold + softness (각 0~255). |

## 릴레이 환경 변수

같은 폴더에 `.env` 파일을 두면 자동으로 로드됩니다. 셸 / systemd에서 export한 환경 변수가 `.env` 값을 덮어씁니다.

| 변수 | 기본값 | 용도 |
| --- | --- | --- |
| `VTCAST_RELAY_BIND` | `0.0.0.0:17239` | HTTP + WebSocket 바인드 주소. |
| `VTCAST_TURN_PORT` | `3478` | TURN UDP 포트. |
| `VTCAST_TURN_PUBLIC_IP` | `127.0.0.1` | 릴레이 공인 IP. 인터넷 호스팅 시 진짜 공인 IP로 설정. 대칭 NAT 피어가 TURN 릴레이 주소로 사용. |
| `VTCAST_TURN_ADVERTISED` | (= `VTCAST_TURN_PUBLIC_IP`) | 클라이언트한테 보내는 `turn:` URL의 호스트네임. 도메인을 쓰면 서버 IP가 바뀌어도 클라이언트 무중단. |
| `VTCAST_TURN_SECRET` | 프로세스마다 랜덤 | TURN credential HMAC 시크릿. **반드시 고정값으로 박아두세요** — 안 그러면 재시작할 때마다 발급된 credential이 전부 무효화됩니다. `openssl rand -hex 32`로 생성. |
| `RUST_LOG` | `info,vtcast_relay=debug` | 로그 필터. |

리포지토리에 포함된 `.env.example`을 복사해서 시작점으로 쓰세요.

## 자체 호스팅 (Linux VPS 예시)

```bash
# 1. Rust 설치
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev

# 2. 빌드
git clone https://github.com/so0420/VTCast.git
cd VTCast
cargo build --release -p vtcast-relay --bin vtcast-relay

# 3. .env 설정
cp .env.example .env
# VTCAST_TURN_PUBLIC_IP, VTCAST_TURN_SECRET 채우기
# 본인 도메인 쓸 거면 VTCAST_TURN_ADVERTISED도 수정
nano .env

# 4. 방화벽
sudo ufw allow 17239/tcp     # 리버스 프록시 안 쓸 때
sudo ufw allow 3478/udp      # TURN (필수)
sudo ufw allow 443/tcp       # 리버스 프록시 쓸 때
```

systemd 서비스로 등록:

```ini
# /etc/systemd/system/vtcast-relay.service
[Unit]
Description=VTCast relay
After=network.target

[Service]
Type=simple
User=youruser
WorkingDirectory=/home/youruser/VTCast
ExecStart=/home/youruser/VTCast/target/release/vtcast-relay
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now vtcast-relay
journalctl -u vtcast-relay -f
```

### 리버스 프록시 (nginx + HTTPS)

릴레이는 TLS를 종료하지 않습니다. 외부에서 `wss://` 로 접속하려면 nginx / Caddy를 앞에 두세요.

nginx 설정 예 (`/etc/nginx/sites-available/vtcast.example.com`):

```nginx
server {
    listen 80;
    server_name vtcast.example.com;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl;
    http2 on;
    server_name vtcast.example.com;

    ssl_certificate     /etc/letsencrypt/live/vtcast.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/vtcast.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:17239;
        proxy_http_version 1.1;

        # WebSocket Upgrade - 이거 빠지면 시그널링 죽음
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;
    }
}
```

**중요**: TURN/UDP 3478은 리버스 프록시(HTTP only)를 거치지 않고 직접 노출됩니다. Cloudflare Proxy(오렌지 구름) 뒤에 두는 경우 TURN용 별도 DNS 레코드를 grey-cloud로 만들고 `VTCAST_TURN_ADVERTISED`를 그쪽으로 가리키세요.

## 프로젝트 구조

```
crates/
  protocol/     serde wire types (Hello / Welcome / Sdp / IceCandidate / ...)
  relay/        axum 시그널링 + 내장 TURN. receiver.html 도 여기서 서빙
  sender/       캡처 → side-by-side 패킹 → 인코딩 → WebRTC 송신
  sender-app/   sender 라이브러리를 감싸는 Tauri 2 데스크톱 GUI
  capture/      순수 Rust Spout / WGC / DDA 캡처 라이브러리
scripts/
  smoke_ws.py   시그널링 smoke test
poc1/           Phase 0 알파 파이프라인 검증 (참고용 보존)
```

## 알려진 제약

- 송신은 Windows에서만 동작 (Spout / WGC / DDA가 Windows-only). 릴레이는 크로스 플랫폼.
- 송신측 외부 인코더로 ffmpeg 백엔드를 쓰는 경우 `ffmpeg.exe`가 PATH에 있어야 함. 없으면 `--backend mf`로 Media Foundation 사용 가능.
- 4K 송신은 권장하지 않음. 1080p가 인코더 / 디코더 / 대역폭 측면에서 sweet spot.
- 모바일 캐리어 NAT 등 대칭 NAT 환경은 TURN fallback이 동작하므로 릴레이의 UDP 3478이 외부에서 도달 가능해야 함.

## 라이선스

MIT. 자세한 내용은 [LICENSE](./LICENSE).

버그 신고 / 기능 제안 / PR은 GitHub Issues / Pull Requests에서 받습니다.
