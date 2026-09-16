# TikTok — Vorcall, vídeo 1 (conta pessoal do dono)

Owner: the founder · Written: 2026-09-15 · Account: the founder's **personal** TikTok.

**Not a promo — a story.** The spine is the August 2026 event every Brazilian gamer lived through: Discord's video features, screen share included, went dark in Brazil. The project is what the owner did about it. No install pitch, no architecture, no explaining what Discord is — the audience already knows.

Craft inherited from `freedom.study/docs/marketing/formato-video-2026-09-13.md`: face on the hook, the screen proves every sentence, dry humour, the last line is one you'd send to someone.

## The fact the video stands on (checked 2026-09-15)

- **12/08/2026** — the ANPD ordered Discord to suspend Go Live and every equivalent video feature in Brazil.
- **17/08/2026** — Discord complied and suspended **all** video features in Brazil: lives, camera and screen share.
- Text and audio were untouched — DMs, group DMs and servers kept working.
- **Janja** publicly said the measure was because Discord "não respeita o ECA Digital".

Sources: [Agência UVA](https://agenciauva.net/2026/08/17/discord-suspende-compartilhamento-de-telas-e-cameras-no-brasil/) · [Poder360](https://www.poder360.com.br/poder-tech/janja-diz-que-punicao-ao-discord-se-deu-por-desrespeito-ao-eca-digital/) · [Discord support](https://support.discord.com/hc/en-us/articles/42704051358359-Why-video-features-are-currently-unavailable-in-Brazil)

**One precision note.** The order came from the ANPD; Janja defended it publicly. Naming her is the popular framing and gets reach, and it also turns the comments into a political fight the project has nothing to do with. Both lines are written below — the owner picks. Everything else in the script works either way.

## Packaging

- **Gancho:** `eu fiz o discord brasileiro` → `e ele se chama vorcall`
- **Card de título:** blur no vídeo → a **animação de entrance** do Vorcall (1,8 s + 0,35 s de hold ≈ 2,1 s).
- **Capa:** o rosto meio borrado com a marca do Vorcall por cima, texto `o discord brasileiro`.
- **Promessa:** a tela compartilhada que sumiu do Brasil em agosto, funcionando.
- **Final escrito primeiro:** "tiraram uma coisa da gente. a gente fez outra."

## Roteiro — 48 s

Pace ≤2,8 palavras faladas por segundo. Texto na tela em minúsculas, ≤8 palavras, safe zone (topo 130 px, base 484 px em 1080×1920).

| Beat | t | Voz | Tela | Texto na tela |
|---|---|---|---|---|
| **gancho** | 0:00–0:04 | "eu fiz o Discord brasileiro." *(bate)* "e ele se chama Vorcall." | **Rosto**, close, sem cenário montado, sem sorrir. | `eu fiz o discord brasileiro` |
| **card** | 0:04–0:06 | *(sem voz — só o som da animação)* | O rosto **desfoca** e escurece; a animação de entrance entra sobre o fundo do splash. | — |
| **1 · o que aconteceu** | 0:06–0:15 | "em agosto tiraram o compartilhamento de tela do Discord aqui no Brasil." *(bate)* "e a Janja foi lá dizer que era pra proteger criança." | C9: o aviso do próprio Discord na tela, rolando. Sem meme, sem foto de ninguém — só o texto oficial. | `17 de agosto de 2026` |
| **2 · o custo** | 0:15–0:20 | "aí meu grupo, que tem [N] adultos que se conhecem faz [X] anos, ficou sem ver a tela um do outro." | C10: o canal de voz do Discord do grupo, com gente dentro e nenhuma tela. | — |
| **3 · a virada** | 0:20–0:23 | "então eu fiz o nosso." *(pausa inteira)* | **Rosto**, uma batida. Silêncio. | — |
| **4 · payoff** | 0:23–0:31 | "aqui a tela vai." *(bate)* "o jogo, o vídeo, o som junto." *(bate)* "e ninguém pode tirar de novo." | C4: o stage com a tela de alguém rodando **de verdade**, o áudio audível na captura. É o pico do vídeo — deixa a imagem respirar. | `com o som junto` |
| **5 · a chamada** | 0:31–0:40 | "já que eu tava mexendo, fiz a chamada direito: sem eco, e quem grita tem um volume só dele." "não tem botão de entrar — é convite na mão, um por um." | C2: a call real, os anéis acendendo → C3: o slider descendo na pessoa que está alta → C6: a lista de convites. | `convite na mão` |
| **6 · a piada** | 0:40–0:44 | "e tem soundpad." *(bate)* "isso foi um erro." | C5: três disparos seguidos por cima de alguém tentando falar. | `um erro` |
| **fecho** | 0:44–0:48 | "tiraram uma coisa da gente." *(bate)* "a gente fez outra." | Rosto meio segundo → último frame = a marca do card de título (loop). | `vorcall` |

**Alternativa sem citar ninguém** (beat 1, mesma duração): "em agosto tiraram o compartilhamento de tela do Discord aqui no Brasil." *(bate)* "não foi bug, não foi manutenção. foi ordem do governo." — a história continua inteira e os comentários continuam sobre o projeto.

**Legenda:** `desde 17 de agosto o discord não tem mais compartilhamento de tela no brasil. então eu fiz o nosso. chama vorcall.`

**Hashtags (3):** `#discord #vorcall #devbr`

**Comentário fixado:** a data e a fonte, nada mais — `17/08/2026, o discord suspendeu todos os recursos de vídeo no brasil por determinação da anpd. é isso que o vídeo tá falando.` Link do projeto só se pedirem.

## A transição do blur (0:04–0:06)

1. A última sílaba de "Vorcall" é o ponto de corte — o blur **começa nela**, não depois.
2. Gaussiano subindo em 6–8 frames com um *scale up* leve (1,00 → 1,06). O rosto sai desfocado e escurecendo, nunca com fade branco.
3. A animação entra já rodando sobre o fundo escuro do splash, centrada no quadro 9:16.
4. Som: um *thump* grave quando a marca assenta. O áudio do beat 1 já começa por baixo do último terço da animação, pra emendar sem buraco.

Grave a animação limpa rodando o cliente com `VORCALL_ENTRANCE=rare` — a variante rara, que normalmente sai 1 vez em 10. É a primeira vez que a maioria vai ver qualquer coisa do Vorcall. Tela cheia, recortada depois pro 9:16.

**Variante que vale testar depois:** mover o card pro fim do beat 3, caindo em cima de "então eu fiz o nosso". Aí a animação deixa de ser abertura e vira a revelação da história. Posta a versão do dono primeiro; essa vira o segundo teste se a retenção cair antes dos 10 s.

## Capturas

| # | Cena | Beat |
|---|---|---|
| C1 | Rosto, luz lateral, nada atrás. Três leituras do gancho, da virada e do fecho. | gancho, 3, fecho |
| C2 | **Uma call de verdade**, com os amigos falando. Nunca um canal vazio com você sozinho. | 5 |
| C3 | O menu de um membro e o slider de volume descendo — na pessoa que está alta no áudio da captura. | 5 |
| C4 | Screen share rodando no stage, com o áudio audível. **É o plano mais importante do vídeo** — grave três tomadas. | 4 |
| C5 | Soundpad: três disparos por cima de alguém tentando falar. | 6 |
| C6 | A tela de convites, criando um e copiando. | 5 |
| C9 | O aviso do próprio Discord sobre os recursos de vídeo no Brasil, rolando na tela. | 1 |
| C10 | O canal de voz do Discord do grupo, com gente dentro e nenhuma tela compartilhada. | 2 |

## Antes de gravar

1. **Os dois números do beat 2 são reais** — quantas pessoas e há quantos anos. Se não souber de cabeça, conta.
2. **A call tem que ser real.** Canal vazio derruba o beat 5.
3. **Peça autorização** antes de deixar nome ou avatar de amigo legível no frame.
4. **Sem foto e sem meme de político** no beat 1. O aviso oficial do Discord é mais forte, e é o que mantém o vídeo sobre o projeto.
5. **Nada de "o Discord acabou"** — foi só o vídeo, e texto e áudio continuam funcionando lá. Um comentário vai corrigir isso em cinco minutos se você exagerar.
6. **"Discord brasileiro"** significa "feito por mim, pro meu grupo". Nada além disso.

## Direção de voz

Duas leituras completas, **seco e baixo**, como quem conta uma coisa pra um amigo na mesa. O beat 1 é dito sem revolta na voz — o fato já é a revolta. A pausa depois de "então eu fiz o nosso" é inteira, uma batida de silêncio; é ela que segura o espectador até o payoff. As duas linhas de graça ("isso foi um erro", "a gente fez outra") saem sem ênfase nenhuma. Celular a um palmo da boca, sem eco.

## QA

Gancho em ≤4 s, sem cumprimento ✓ · a história inteira dita antes dos 23 s ✓ · o payoff é a coisa que tiraram, funcionando ✓ · troca visual a cada ≤3–5 s ✓ · toda frase tem correspondente na tela ✓ · pace ≤2,8 palavras/s ✓ · data e fatos conferem com a seção do topo ✓ · nenhuma explicação do que é Discord ✓ · master nunca mudo ✓ · legenda palavra a palavra do Whisper ✓ · último frame emenda no primeiro ✓ · sem CTA de instalação ✓.

## Se esse funcionar, os dois próximos

1. **"respondendo: que app é esse?"** — 15 s, formato de resposta a comentário. É onde o nome e o resto cabem sem virar anúncio.
2. **"o soundpad foi o maior erro da minha vida"** — 30 s, só a piada, o app como antagonista.
