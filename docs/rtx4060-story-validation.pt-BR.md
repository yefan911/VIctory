# VIctory na RTX 4060 — relatório de validação do modo história

[English version](rtx4060-story-validation.md)

**Status: avanço parcial de compatibilidade; modo história ainda não jogável.** Este relatório registra duas execuções em hardware e um experimento posterior com shaders sintéticos. Não é um novo patch upstream nem uma distribuição de binários.

## Configuração testada

- Windows 11, NVIDIA GeForce RTX 4060 e D3D12 por hardware, sem WARP nas execuções do jogo. O contador 3D do Windows confirmou atividade da GPU no processo de referência.
- Perfil Xbox Series S, saída em 1920×1080, janela de apresentação própria, descarte de shaders mesh Wave64 e DRED solicitado. A branch do relatório parte do upstream `ffe310e`; a integração local usada nos testes partia de `8f5e08a` e já incluía mudanças na apresentação e em Wave64, que não são publicadas aqui.
- O build corrigido incorporou apenas as mudanças de formato relevantes do [PR #1076](https://github.com/Marvisak/VIctory/pull/1076), commits `e426feb` e `44cbed0`, sem substituir a integração local pelo fork inteiro.

## Log de execução — 24/11/2026 (UTC)

| Evento | Referência em GPU | Build com correção de texturas |
|---|---|---|
| Primeiro `Present` com `S_OK` | 16:38:32, 1920×1080 | 16:47:43, 1920×1080 |
| Falhas ao criar texturas no host | 4, todas `0x80070057` | 0 |
| Último marco periódico de apresentação | 1024 | 2048 |
| Transições para preto registradas pelo detector | Quadros 1, 917, 1638, 1640 | Quadros 1, 3542, 3852, 3854 |
| Avisos `shader does not translate` | 292 | 249 |
| Avisos `a descriptor read only as data` | 166 | 129 |
| Recusas de Wave64 em grupos com mais de 64 threads | 26 | 26 |
| Falha final da GPU | 16:41:54, `DXGI_ERROR_DEVICE_HUNG (0x887A0006)` | 16:55:47, `DXGI_ERROR_DEVICE_HUNG (0x887A0006)` |

- As quatro recusas originais de textura — pares `(formato DXGI, flags)` `(67, 5)`, `(67, 4)`, `(86, 5)` e `(115, 1)` — não reapareceram no build corrigido. Isso não comprova suporte a todos os formatos nem a correção do conteúdo das texturas.
- No build corrigido, apareceram elementos do menu, a arte de abertura e legendas da sequência inicial do modo história. Na transição observada, a cena ao fundo estava preta. **Não foi verificado mundo 3D corretamente renderizado nem personagem visível e controlável.**
- O último marco periódico de apresentação não é o último quadro executado. As contagens acima são linhas de aviso, não uma contagem de shaders únicos. São execuções únicas, com perfis de build e trajetórias diferentes; os números não demonstram ganho de desempenho nem maior cobertura do tradutor.
- Os dois logs terminam com falha do dispositivo após uma submissão reportada como contendo uma lista de comandos. Isso não identifica o shader responsável e, por si só, não sustenta que dividir submissões com várias listas resolva o problema. A primeira amostra do log de referência não incluía a falha posterior.
- O relatório é sanitizado: não contém logs brutos, endereços de memória, dumps, capturas do jogo, arquivos proprietários, configuração local ou credenciais. Consulte a [política de assets](asset-policy.md).

## Reprodução e testes

1. No Windows, mantenha `VICTORY_D3D12_WARP` desativado (variável ausente) e use a GPU de hardware. Compile com `cargo build --locked --profile diag -p victory --features diagnostics`. Use o executável fornecido e configurado localmente, com `--execute --console series-s` e `VICTORY_D3D12_DRED=1`. Para comparar com estas execuções, mantenha a resolução de 1080p e a janela própria da integração local. Registre separadamente inicialização, transição da história e eventual perda do dispositivo.
2. Na integração com a correção de textura, execute `cargo test --locked --profile diag -p victory-gpu --features diagnostics -- --test-threads=1`. O teste de regressão falhava no formato 67 com flags 5 e passou após incorporar a correção. Suíte GPU: **75 aprovados, 0 falhas, 1 ignorado**. Sem `--features diagnostics`, o build dos testes dessa integração encontrou `E0560` no campo `Shader.wave64`; trata-se de um problema separado de configuração de testes.
3. O teste de descritor com tipo explícito do [PR #1067](https://github.com/Marvisak/VIctory/pull/1067) já passava na integração local. Uma variante sintética, sem o intervalo que declara o tipo, reproduziu `a descriptor read only as data`. Um ajuste experimental passou a aceitar somente descritores não declarados de oito palavras completas, tratados como SRV 2D. O teste passou, a suíte de shaders registrou **193 aprovados, 0 falhas, 1 ignorado** e o binário de diagnóstico compilou. **O experimento não foi validado em jogo** e não comprova tratamento correto de cube maps, texturas 3D, arrays ou UAVs, nem resolve comprovadamente o travamento.

## Trabalho existente e contribuição deste relatório

| Referências da comunidade | Resultado local e limite |
|---|---|
| [#1076](https://github.com/Marvisak/VIctory/pull/1076), [#1092](https://github.com/Marvisak/VIctory/pull/1092), [#735](https://github.com/Marvisak/VIctory/issues/735) — formatos de textura e compatibilidade NVIDIA | Teste de regressão independente e execução em Series S sem as quatro recusas, mas ainda com perda do dispositivo. O patch de texturas não é republicado. |
| [#1067](https://github.com/Marvisak/VIctory/pull/1067) e [#1130](https://github.com/Marvisak/VIctory/pull/1130) — leituras de descritores como dados | Validação sintética de um caso 2D sem tipo declarado. O código local não é publicado porque se sobrepõe aos PRs existentes. |
| [#439](https://github.com/Marvisak/VIctory/pull/439) e [#1132](https://github.com/Marvisak/VIctory/issues/1132) — tradução de Wave64 | A recusa para grupos acima de 64 threads permanece. A correção de textura não é uma correção de Wave64. |
| [#1133](https://github.com/Marvisak/VIctory/pull/1133) e [#1081](https://github.com/Marvisak/VIctory/pull/1081) — heap de descritores, barreiras e breadcrumbs | Próxima comparação: repetir o cenário com a mesma configuração em hardware e usar DRED/breadcrumbs para isolar a etapa que trava. Ainda não há prova de que esses PRs resolvam esta falha. |

**Critério ainda em aberto:** renderizar o cenário e o personagem em hardware, confirmar que um comando movimenta o personagem e atravessar a transição inicial sem perder o dispositivo. Nenhum build testado atendeu a esses critérios.
