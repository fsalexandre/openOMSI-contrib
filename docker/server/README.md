# Servidor dedicado openOMSI (Linux, Docker)

O servidor é o próprio `openomsi` iniciado com `--server`: sem janela, sem som e sem placa de
vídeo. Este container o compila para Linux e o roda com a sua instalação do OMSI 2 montada
**somente leitura**. Serve tanto para testar localmente (Docker Desktop no Windows) quanto
para instalar num servidor Linux.

## Requisitos

- Docker com Compose (Docker Desktop no Windows; `docker` + `docker compose` no Linux).
- Uma instalação **completa** do OMSI 2 (a mesma que os jogadores têm: mapas, ônibus originais).
  No Linux, copie a pasta do OMSI 2 para o servidor (ex.: `/srv/omsi2`).

## Instalar e iniciar

Nesta pasta (`docker/server`):

```sh
cp .env.example .env              # Windows: copy .env.example .env
#   edite .env: OMSI_DIR = a pasta do OMSI 2 (a que tem Omsi.exe, maps e Vehicles)
mkdir config
cp server.cfg.example config/server.cfg   # opcional: sem ele o servidor escreve um com os padrões
docker compose up -d --build      # a primeira compilação leva vários minutos
docker compose logs -f            # acompanhe: "server '...': map ..." e depois o mapa carregado
```

O servidor está pronto quando `http://<endereço>:27025/status` responde (o Docker também mostra
`healthy` em `docker ps`).

### Portas

| Porta | Protocolo | Para quê |
|---|---|---|
| 27015 | UDP | a sessão (posições, eventos) |
| 27015 | TCP | mods do mapa e **ônibus dos jogadores** (envio e download por conteúdo) |
| 27025 | TCP | web: `/status`, ícone, jogadores via WebSocket |

Num servidor na internet, libere as três no firewall.

## Entrar no servidor (jogadores)

No launcher: **Multiplayer → Join**, endereço `<endereço do servidor>:27015` (localmente:
`127.0.0.1:27015`). Ou pela linha de comando:
`openomsi.exe --lan-join 127.0.0.1:27015 --bus Vehicles/... --map maps/Grundorf/global.cfg`.

Todos os jogadores precisam desta versão do jogo (branch `custom`) para enviar e receber os
ônibus por conteúdo.

## Configurar o servidor (mapa, nome, tráfego)

```powershell
.\configurar-servidor.ps1
```

Um menu: **1) Trocar mapa** lista os mapas da sua pasta do OMSI 2. O escolhido vai para o
servidor com tudo o que ele usa (objetos, splines, carros do tráfego, pessoas, texturas do
chão), na pasta `content` desta pasta. Depois o `server.cfg` é ajustado e o servidor é
reiniciado. No mesmo disco do OMSI os arquivos entram como links físicos, sem ocupar espaço de
novo. As outras opções mudam nome e mensagem, tráfego e número de jogadores, reiniciam e
mostram o status.

O servidor **não entrega mapas** aos jogadores (`share_maps = 0` no `server.cfg`, o padrão): um
mapa grande tem vários GB e acabaria com a banda do servidor. Cada jogador instala o mapa por
conta própria; quem entrar sem ele é avisado de qual mapa falta. Os ônibus dos jogadores
continuam sendo distribuídos pelo servidor.

Arquivos que o mapa cita e que não existem na sua máquina ficam listados em
`content\openomsi-map-pack-<mapa>.txt`: o servidor e os jogadores também não os terão.

Para um servidor em outra máquina, gere um `.zip` e descompacte-o na pasta de conteúdo de lá:
`openomsi.exe --root "<pasta do OMSI 2>" --pack-map "<nome do mapa>" --pack-to mapa.zip`.

## Ônibus dos jogadores

- Quem entra com um ônibus que o servidor não tem **envia** o ônibus na tela de carregamento
  ("Enviando seu ônibus ao servidor", com progresso). Só vai o que o servidor não tem: arquivos
  que ele tem na própria instalação, na mesma versão, não são enviados.
- Os outros jogadores veem o ônibus **exatamente como o motorista tem** (mesmos arquivos,
  mesma pintura, mesmo `.cti`): da própria cópia, se for idêntica, ou baixado do servidor.
  Enquanto isso, uma **caixa cinza do tamanho do ônibus** com "carregando o ônibus: 40%".
- Nada é gravado nas pastas `Vehicles` de ninguém; o `.cti` e as pinturas de cada jogador
  continuam os dele.
- O servidor guarda os ônibus no volume `openomsi-data` (`/root/.openomsi/lan-store`), todas as
  versões. Ônibus que ninguém usa há `uploads_keep_days` dias (30) saem; com o disco curto, os
  menos usados saem primeiro. Nenhum ônibus é recusado pelo tamanho.

## Teste automático de ponta a ponta (Windows)

Com o servidor rodando e o cliente compilado (`cargo build --release -p omsi-app`):

```powershell
scripts\e2e-bus-sync.ps1 -OmsiDir "D:\SteamLibrary\steamapps\common\OMSI 2"
```

Dois jogos sem janela entram no servidor: A com uma cópia modificada do SD77 e B com um SD80
original. O script confere nos logs que o servidor recebeu só o arquivo diferente, que B baixou
o ônibus de A pelo conteúdo e que A reconheceu o SD80 de B como idêntico.

## Comandos úteis

```sh
docker compose ps                 # estado (healthy quando o /status responde)
docker compose logs -f            # log do servidor
docker compose restart            # reiniciar (os ônibus guardados continuam)
docker compose down               # parar (o volume com os ônibus continua)
docker compose down -v            # parar e APAGAR os ônibus guardados
curl http://127.0.0.1:27025/status
```
