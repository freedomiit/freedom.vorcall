# Hospedando o seu próprio Vorcall

[English](self-hosting.md) · **Português (BR)**

Tudo o que é preciso para rodar um servidor Vorcall para o seu grupo: a primeira
instalação, como colocá-lo na internet, cada configuração, backups e atualizações.

- [Início rápido](#início-rápido)
- [Conectando os clientes](#conectando-os-clientes)
- [Colocando na internet](#colocando-na-internet)
- [Configuração](#configuração)
- [Administração do dia a dia](#administração-do-dia-a-dia)
- [Backups](#backups)
- [Atualizando o servidor](#atualizando-o-servidor)
- [Atualizações do cliente](#atualizações-do-cliente)
- [Monitoramento e logs](#monitoramento-e-logs)
- [Resolvendo problemas](#resolvendo-problemas)

---

## Início rápido

**Requisitos:** uma máquina Linux com Docker e o plugin Compose, aproximadamente 1 GB de
memória livre e 2 GB de disco para começar. A compilação pede um pouco mais; uma VPS de
1 vCPU / 1 GB roda, mas compila devagar.

```bash
git clone https://github.com/freedomiit/freedom.vorcall.git
cd freedom.vorcall
docker compose up -d
```

A primeira execução compila a imagem do backend a partir do código, o que leva alguns
minutos. Ao terminar, a stack aplica as próprias migrações, cria um servidor com o cargo
`@everyone` e uma categoria `General` contendo um canal de texto `general` e um canal de
voz `General`, e gera dois segredos para os quais não havia padrão seguro.

Acompanhe a subida:

```bash
docker compose logs -f backend
```

### Leia a chave de porta

A API do Vorcall é protegida por uma **chave de porta** pré-compartilhada, que todo cliente
envia em toda requisição. Ela não é a senha de uma conta — todo mundo que conecta no seu
servidor usa a mesma — mas sem ela ninguém alcança o servidor.

```bash
docker compose logs backend | grep "Server key"
```

```
[22:17:07 WRN]  No Vorcall:ServerKey configured, so one was generated and saved in
/data/server-key. Give it to everyone who connects — it goes in the client's Server
section. Server key: fb8398eff26d83640069ddd02cb6e3c3
```

Ela fica no volume `data` e é estável entre reinícios. Se preferir escolhê-la, defina
`VORCALL_SERVER_KEY` antes do primeiro `up -d` (veja [Configuração](#configuração)).

### Crie o primeiro convite

Não existe cadastro aberto. Contas só existem contra um código de convite de uso único, e
**a primeira conta a se registrar vira a dona do servidor** — a única identidade que passa
por cima de qualquer verificação de permissão.

```bash
docker compose exec backend dotnet Vorcall.Server.dll invites new
```

```
Invite code: F6M5F-J5GK4-Y4YP4-XG5PW
Expires: 2026-09-19T22:17:31Z
```

Registre-se com ele pelo cliente e aquela conta será a dona. Depois disso, gere um convite
por pessoa.

---

## Conectando os clientes

Instale o cliente pelos
[Releases](https://github.com/freedomiit/freedom.vorcall/releases/latest) e, na tela de
entrada:

1. Clique em **Server** para abrir a seção.
2. Coloque o endereço do seu servidor no primeiro campo — `http://192.168.1.10:5000` numa
   rede local, `https://chat.exemplo.org` depois que tiver um domínio.
3. Coloque a chave de porta no segundo.
4. **Use this server** e então **Create account** com o código de convite.

Os dois valores ficam salvos no `config.toml`, junto das outras preferências, então isso é
passo único por máquina. **Built-in server** devolve o cliente ao endereço que veio
compilado nele.

Duas variáveis de ambiente, `VORCALL_SERVER_URL` e `VORCALL_SERVER_KEY`, têm prioridade
sobre os valores salvos e servem para instalações automatizadas. Quando qualquer uma delas
está definida, os campos aparecem somente para leitura, porque salvá-los não mudaria nada.

> **Um servidor em HTTP puro não é criptografado.** Tudo bem numa rede local ou sobre uma
> VPN como Tailscale ou WireGuard. Em qualquer outro lugar, ponha TLS na frente.

---

## Colocando na internet

Por padrão a API é publicada em `127.0.0.1:5000`, alcançável somente da própria máquina. É
o formato certo: termine o TLS num proxy reverso no host e deixe que ele fale com o backend
pelo loopback.

O **relay de voz é diferente**. Ele é UDP na porta 5005, publicado em todas as interfaces, e
não pode passar por proxy reverso — o nginx não carrega mídia UDP, e os clientes falam com
aquela porta diretamente. Ela precisa estar aberta no seu firewall e no grupo de segurança
do seu provedor. Voz e compartilhamento de tela simplesmente não funcionam sem ela.

### O que abrir

| | Porta | Protocolo | Exposição |
| --- | --- | --- | --- |
| API + WebSocket | 443 | TCP | Pública, pelo proxy reverso |
| Desafio ACME | 80 | TCP | Pública, para emitir o certificado |
| Relay de voz | 5005 | UDP | Pública, direto para o contêiner |
| Backend | 5000 | TCP | Só loopback |
| PostgreSQL | 5433 | TCP | Só loopback |

### nginx

A configuração completa do site está no
[guia em inglês](self-hosting.md#nginx) — copie de lá e troque `chat.example.org` pelo seu
domínio. Emita o certificado antes com
`certbot certonly --webroot -w /var/www/certbot -d chat.exemplo.org`.

Os blocos `location` sem buffer não são opcionais: uploads, arquivos por streaming e
downloads de atualização são corpos grandes entregues aos poucos, e um proxy `/api/`
genérico aplicaria o limite padrão de 1 MiB do nginx e carregaria arquivos inteiros na
memória. Cada um deles define o próprio `client_max_body_size`, porque os três caminhos de
upload têm tetos diferentes. São eles:

- `location /api/attachments` — `client_max_body_size 2g`,
  `proxy_request_buffering off`, `proxy_buffering off`.
- `location /api/images` — o mesmo, com `client_max_body_size 9m`: avatares, banners e
  ícones são um acervo à parte, bem menor.
- `location /api/streams` — o mesmo, com `client_max_body_size 0`: o trecho que o cliente
  remetente envia é do tamanho do arquivo que ele escolheu, e nenhum dos dois sentidos
  pode ser bufferizado. Mantenha o `proxy_read_timeout` bem acima de
  `Vorcall__StreamSenderTimeoutSeconds`: o `GET` de quem lê não produz byte nenhum até o
  cliente remetente responder.
- `location /api/diagnostics` — `client_max_body_size 5m`, também sem buffer.
- `location /api/updates/` — sem buffer, `proxy_read_timeout 300s`.
- `location = /ws` — upgrade de WebSocket, com timeouts de 3600s.
- `location /api/admin/ { return 404; }` — os endpoints de admin são só para a CLI na rede
  do compose.

### O firewall

O relay UDP precisa de um furo no firewall do host e, num provedor de nuvem, no grupo de
segurança. Esse é de longe o motivo mais comum de a voz falhar numa instalação que no resto
funciona.

```bash
# ufw (Debian/Ubuntu)
sudo ufw allow 5005/udp

# firewalld (Fedora/RHEL)
sudo firewall-cmd --permanent --add-port=5005/udp && sudo firewall-cmd --reload
```

Na AWS, GCP, Azure, Oracle Cloud ou Hetzner, adicione também uma regra de entrada para UDP
5005 no painel — o firewall do host sozinho não basta.

### Buffers do kernel

O relay pede buffers de socket de 8 MiB. Sem levantar o teto do kernel, o pedido é cortado
em silêncio, o que aparece como áudio picotado quando várias pessoas falam:

```bash
printf 'net.core.rmem_max = 16777216\nnet.core.wmem_max = 16777216\n' \
  | sudo tee /etc/sysctl.d/90-vorcall.conf
sudo sysctl --system
docker compose restart backend
```

---

## Configuração

Toda configuração é opcional. Coloque o que quiser mudar num arquivo `.env` ao lado do
`docker-compose.yml`; o [`.env.example`](../.env.example) é uma cópia comentada para
começar.

```bash
cp .env.example .env
$EDITOR .env
docker compose up -d
```

### No nível do compose

| Variável | Padrão | O que faz |
| --- | --- | --- |
| `VORCALL_BIND` | `127.0.0.1` | Interface em que a API é publicada. `0.0.0.0` a expõe direto — só faça isso se nada estiver na frente. |
| `VORCALL_PORT` | `5000` | Porta da API no host. |
| `VORCALL_VOICE_PORT` | `5005` | Porta do relay UDP, no host e no contêiner. |
| `VORCALL_SERVER_KEY` | *gerada* | A chave de porta. Defina para escolher a sua em vez de ler a gerada. |
| `VORCALL_JWT_SIGNING_KEY` | *gerada* | Base64 de 32 bytes aleatórios. Trocar desconecta todo mundo em até 15 minutos. |
| `VORCALL_ADMIN_KEY` | *vazia* | Liga os endpoints de admin que `users kick` / `disable` usam. `openssl rand -hex 32`. |
| `VORCALL_VOICE_HOST` | *vazia* | Host para onde os clientes mandam mídia. Vazio significa "o mesmo host do WebSocket", que é o certo a não ser que o relay fique em outro lugar. |
| `VORCALL_SHARE_ENABLED` | `true` | `false` desliga o compartilhamento de tela no servidor inteiro; a voz continua. |
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `vorcall` | Credenciais do banco, publicado só no loopback. |
| `POSTGRES_PORT` | `5433` | Porta loopback do PostgreSQL, para a suíte de testes e ferramentas locais. |

### Configurações do backend

Passe qualquer uma delas ao serviço `backend` como variável de ambiente. Repare no
underscore **duplo** — é assim que o .NET mapeia `Vorcall__ShareMaxKbps` para a
configuração `Vorcall:ShareMaxKbps`.

| Variável | Padrão | O que faz |
| --- | --- | --- |
| `Vorcall__VoiceEnabled` | `true` | `false` desliga a voz por completo; `JoinVoice` responde `VOICE_UNAVAILABLE`. |
| `Vorcall__ShareMaxKbps` | `30000` | Teto por sessão para a mídia de compartilhamento. Faixa 1000–200000. |
| `Vorcall__MaxSharersPerRoom` | `3` | Quantas pessoas podem compartilhar num canal ao mesmo tempo. Faixa 1–16. |
| `Vorcall__AttachmentsMaxBytes` | `214748364800` | Cota total em disco para uploads, 200 GiB. Acima dela, um upload é recusado com 507. Ajuste ao tamanho real do seu disco. |
| `Vorcall__StreamsEnabled` | `true` | `false` desliga os arquivos por streaming no servidor inteiro e desmapeia as quatro rotas `/api/streams`. |
| `Vorcall__StreamSenderTimeoutSeconds` | `30` | Quanto tempo quem lê espera a resposta do cliente remetente antes de o download falhar com 504. Faixa 1–600. |
| `Vorcall__StreamMaxTransfersPerOwner` | `8` | Quantas transferências uma conta pode servir ao mesmo tempo. Faixa 1–64. |
| `Vorcall__AuthRequestsPerWindow` | `10` | Requisições de login e cadastro por minuto por IP. |
| `Vorcall__UploadRequestsPerWindow` | `20` | Uploads por minuto por conta. |
| `Vorcall__MessageBurst` | `20` | Quadros de escrita que uma conta pode mandar em sequência. |
| `Vorcall__MessagesPerSecond` | `2` | Taxa sustentada de quadros de escrita por conta. |
| `Vorcall__DiagnosticsReportsPerHour` | `10` | Relatórios de problema por conta por hora. |
| `Vorcall__DataDir` | `/data` | Onde ficam os segredos gerados. |
| `Vorcall__AttachmentsDir` | `/attachments` | Arquivos enviados, e os avatares, banners e ícones ao lado deles. |
| `Vorcall__LogsDir` | `/logs` | Logs JSON diários, 31 mantidos. |
| `Vorcall__DiagnosticsDir` | `/diagnostics` | Relatórios de problema, varridos após 30 dias. |
| `Vorcall__ReleasesDir` | `/releases` | Versões assinadas do cliente, servidas em `/api/updates/*`. |

Não é configurável: um anexo pode ser um arquivo de qualquer tipo, de até 2 GiB, quatro por
mensagem. Um arquivo maior que isso é enviado como **arquivo por streaming** — o servidor
guarda a oferta e repassa os bytes, mas nunca os armazena, de modo que ele só pode ser lido
enquanto o cliente de quem enviou estiver online; esses param em 1 TiB, também quatro por
mensagem. Avatares, banners e ícones são um acervo à parte, mais rígido: só PNG, JPEG, GIF
ou WebP, conferidos contra o número mágico do tipo, 8 MiB cada. Só os anexos e as imagens
contam para `Vorcall__AttachmentsMaxBytes`; um arquivo por streaming não ocupa disco nenhum
aqui.

### Onde ficam os dados

Os volumes Docker, todos criados no primeiro `up -d`:

| Volume | Contém | Fazer backup? |
| --- | --- | --- |
| `pgdata` | Contas, mensagens, canais, cargos — tudo | **Sim** |
| `data` | A chave de porta e a chave de assinatura geradas | **Sim** |
| `attachments` | Arquivos enviados, avatares, banners, ícones | **Sim** |
| `logs` | Logs JSON diários | Não |
| `diagnostics` | Relatórios de problema dos clientes | Não |
| `releases` | Versões assinadas do cliente, se você publicar alguma | Se usar |

---

## Administração do dia a dia

O binário do servidor também é a ferramenta de administração. Todo subcomando roda as
migrações pendentes e sai, sem nunca subir o servidor web.

```bash
docker compose exec backend dotnet Vorcall.Server.dll <subcomando>
```

```
invites new [--days N]        código de convite de uso único, 7 dias por padrão
invites list                  usados / revogados / expirados
invites revoke <id>           mata um código não usado, definitivamente

users list                    com versão do cliente, plataforma e último acesso
users disable <usuário>       tranca a conta e revoga as sessões dela
users enable <usuário>        destranca
users kick <usuário>          fecha a conexão ao vivo
users revoke-sessions <usuário>
users set-password <usuário>  pede uma senha nova

server show                   nome, dono e id do canal geral
server set-owner <usuário>    entrega o servidor àquela conta
```

`users kick`, `disable` e `enable` precisam de `VORCALL_ADMIN_KEY` definida para o passo de
derrubar a conexão na hora; sem ela a CLI avisa e a tranca vale mesmo assim, em até 30
segundos.

Uma conta **desativada** é uma tranca de operador — desfeita com `users enable`. Um
**banimento**, aplicado de dentro do app por alguém com a permissão, é outra coisa: grava
uma linha de banimento, apaga toda mensagem que a conta enviou, remove as sobrescritas dela
e a tira do servidor.

Referência completa: [docs/administration.md](administration.md) (em inglês).

---

## Backups

O que importa é o `pgdata`; `data` e `attachments` também valem a pena. Este script faz o
dump do banco, guarda 14 dias e pode rodar com o servidor no ar:

```bash
#!/usr/bin/env bash
# /usr/local/bin/vorcall-backup
set -euo pipefail
APP_DIR=/opt/vorcall            # onde você clonou
cd "$APP_DIR"
set -a; . .env 2>/dev/null || true; set +a
mkdir -p backups
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
docker compose exec -T db pg_dump -Fc \
  -U "${POSTGRES_USER:-vorcall}" -d "${POSTGRES_DB:-vorcall}" \
  > "backups/vorcall-$STAMP.dump"
find backups -name 'vorcall-*.dump' -mtime +14 -delete
```

Toda noite, via cron:

```
15 3 * * * root /usr/local/bin/vorcall-backup >> /var/log/vorcall-backup.log 2>&1
```

Faça backup também dos segredos e dos uploads — perder o `data` troca a chave de porta e
desconecta todo mundo:

```bash
docker run --rm -v freedomvorcall_data:/src:ro -v "$PWD/backups:/out" \
  alpine tar czf /out/data.tar.gz -C /src .
docker run --rm -v freedomvorcall_attachments:/src:ro -v "$PWD/backups:/out" \
  alpine tar czf /out/attachments.tar.gz -C /src .
```

### Restaurando

**Destrutivo** — derruba e recria tudo o que está no dump. Pare o backend antes:

```bash
docker compose stop backend
set -a; . .env 2>/dev/null || true; set +a
docker compose exec -T db pg_restore \
  -U "${POSTGRES_USER:-vorcall}" -d "${POSTGRES_DB:-vorcall}" \
  --clean --if-exists --no-owner < backups/vorcall-<stamp>.dump
docker compose start backend
curl -fsS localhost:5000/health
```

---

## Atualizando o servidor

```bash
git pull
docker compose up -d --build
```

O backend aplica as próprias migrações ao subir, e `/health` só responde 200 depois que
isso termina. **Faça um dump do banco antes de atualizar** — uma migração é de mão única.

---

## Atualizações do cliente

O atualizador embutido do Vorcall busca **no servidor ao qual o cliente está conectado**,
em `/api/updates/manifest`. Um servidor auto-hospedado não serve nada ali a menos que você
coloque um manifesto assinado no volume `releases`, então, por padrão, os clientes do seu
pessoal vão informar que não há atualização.

Duas formas de lidar com isso:

- **Aponte as pessoas para os Releases do GitHub.** O mais simples. Avise quando sair uma
  versão nova e deixe que rodem o instalador de novo; ele preserva as configurações e o
  servidor salvo.
- **Publique as suas próprias versões assinadas.** Gere uma chave com `vorcall-release
  gen-key`, embuta a metade pública em `client/update-keys.pub`, compile os seus clientes e
  sirva o manifesto assinado pelo volume `releases`. Veja
  [docs/releasing.md](releasing.md). Isso significa compilar e distribuir o seu próprio
  cliente, o que só compensa para um grupo grande.

Clientes compilados deste repositório com chave de desenvolvimento nunca se atualizam.

---

## Monitoramento e logs

**Logs.** O backend escreve na saída padrão do contêiner e em arquivos JSON diários no
volume `logs`, 31 mantidos. Cada linha de WebSocket carrega o id da sessão, o id e o nome
do usuário; cada requisição HTTP gera uma linha. Tokens, senhas, códigos de convite e a
chave de assinatura nunca são registrados.

```bash
docker compose logs -f backend
```

**Métricas.** `GET /metrics` devolve uma exposição no formato Prometheus. Só é servida a
origens de loopback e de faixas privadas, e nunca passa pelo proxy, então não precisa de
chave:

```bash
curl -s localhost:5000/metrics
```

**Saúde.** `GET /health` responde 200 quando as migrações terminaram e o banco responde,
503 caso contrário.

---

## Resolvendo problemas

**`docker compose up -d` termina, mas o backend fica reiniciando.**
`docker compose logs backend`. Em geral o banco não estava pronto (o healthcheck deveria
evitar isso) ou uma migração falhou. Uma migração que falha deixa a porta fechada de
propósito.

**Os clientes são recusados antes mesmo da tela de entrada.**
Chave de porta errada. Releia com `docker compose logs backend | grep "Server key"` e
confira a seção Server do cliente.

**O login funciona, mas a voz não conecta.**
A UDP 5005 não está acessível. Confira o firewall do host *e* o grupo de segurança da
nuvem. Teste com `nc -u -z -v <seu-servidor> 5005` de outra máquina.

**A voz conecta, mas fica picotada com várias pessoas.**
O kernel cortou os buffers de socket do relay. Aplique os
[ajustes de sysctl](#buffers-do-kernel) e reinicie o backend.

**Os uploads falham por volta de 1 MB, ou os maiores falham em 9 MB.**
O proxy reverso está aplicando um limite de corpo. Os locations `/api/attachments`,
`/api/images` e `/api/streams` precisam cada um do seu próprio `client_max_body_size` —
2 GiB, 9 MB e sem limite, respectivamente — e de proxy sem buffer. Uma configuração escrita
antes da 0.6.0 limita todos eles a `9m`, o que barra todo anexo acima disso e todo arquivo
por streaming.

**O compartilhamento de tela começa e para na hora, no Linux.**
O cliente precisa do PipeWire (`libpipewire-0.3.so.0`) e de um portal de desktop. Todo
compartilhamento abre o seletor do portal por design — nenhuma fonte é lembrada.

**Ninguém consegue fazer nada, nem a primeira conta.**
O servidor está sem dono. `server show` confirma; `server set-owner <usuário>` resolve, e
vale a partir do próximo reinício do backend.

**Perdi o volume `data`.**
A chave de porta e a chave de assinatura se foram. Novas são geradas no próximo boot: todo
mundo é desconectado e todo mundo precisa da chave nova. As contas e mensagens em `pgdata`
não são afetadas.
