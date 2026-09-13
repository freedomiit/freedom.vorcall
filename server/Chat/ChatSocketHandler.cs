using System.Net.WebSockets;
using Google.Protobuf;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Admin;
using Vorcall.Server.Attachments;
using Vorcall.Server.Data;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Streams;

namespace Vorcall.Server.Chat;

// Implements the per-connection session state machine of PROTOCOL.md: AwaitingHello with a
// 5 s deadline, then Ready with a 120 s idle deadline.
//
// Every frame is parsed, normalised and answered here; the work itself belongs to the registry
// (channels, categories, roles, members, voice and share), to MessageService (the message rows) or
// to the channel directory (read cursors). Those never see an unnormalised name, a non-positive id
// or a caller whose permissions were not resolved, and they own their own broadcasts: this handler
// broadcasts for messages alone.
public sealed class ChatSocketHandler(
    ConnectionRegistry registry,
    MessageService messages,
    ChannelDirectory channels,
    AttachmentStore attachments,
    ImageStore images,
    DisabledAccounts accounts,
    IDbContextFactory<AppDbContext> contextFactory,
    IHostApplicationLifetime lifetime,
    IConfiguration configuration,
    ILogger<ChatSocketHandler> logger)
{
    private const int MaxInboundBytes = 16 * 1024;
    private const int MaxClientDescriptionLength = 64;
    private const int ReceiveBufferSize = 4 * 1024;
    private const uint SupportedProtocolVersion = 1;

    // 1008 for every protocol violation, 1001 for idle and shutdown.
    private const WebSocketCloseStatus ProtocolClose = WebSocketCloseStatus.PolicyViolation;
    private const WebSocketCloseStatus GoingAwayClose = WebSocketCloseStatus.EndpointUnavailable;

    // Shared with Program's shutdown hook so both close paths report the same reason.
    public const string ShutdownReason = "server shutting down";

    // Id 0, an id nothing names and a channel the caller may not view are one answer on purpose: a
    // hidden channel must be indistinguishable from a missing one.
    private const string UnknownChannelDetail = "unknown channel";
    private const string InvalidTextDetail = "text must be 1..2000 characters after trimming";
    private const string InvalidAttachmentDetail = "attachment is unknown, not yours, not in this channel or already used";
    private const string InvalidStreamDetail = "streamed file is unknown, not yours, not in this channel or already used";
    private const string UnknownMessageDetail = "unknown or deleted message";
    private const string InvalidNameDetail = "name must be 1..32 characters without control characters";
    private const string NotInVoiceDetail = "join the voice channel first";
    private const string RateLimitedDetail = "too many messages, slow down";

    // The eight of PROTOCOL.md, compared as exact strings: the heart carries its variation
    // selector, so a client that drops it is not sending one of these.
    private static readonly string[] AcceptedReactions = ["👍", "❤️", "😂", "😮", "😢", "🔥", "🎉", "👀"];

    // One instance per connection, so this reads the configuration once per session; the values
    // are fixed for the process either way, and an unusable one throws here rather than silently
    // leaving the writes unlimited.
    private readonly ChatLimits _limits = ChatLimits.FromConfiguration(configuration);

    private static readonly TimeSpan HelloDeadline = TimeSpan.FromSeconds(5);
    private static readonly TimeSpan IdleDeadline = TimeSpan.FromSeconds(120);

    // An accepted mutation always runs to completion: it is already authorised, its broadcast
    // reaches everyone who may see it, and a write abandoned halfway would leave the registry's
    // mirror describing a row the database never got. The caller's socket dying is no reason to
    // stop — MessageService's writes take no token at all for the same reason.
    private static readonly CancellationToken Persist = CancellationToken.None;

    // The bearer token of the upgrade request already identified the caller, so the handshake
    // only has to agree on the protocol version.
    public async Task HandleAsync(WebSocket socket, long userId, string username, CancellationToken requestAborted)
    {
        var connection = new ClientConnection(socket, logger, requestAborted, lifetime.ApplicationStopping);
        var reader = new SocketReader(connection, logger);

        // Everything this session logs from here down — the registry, the services it awaits —
        // carries who and which socket, so one friend's session can be read out of a log file
        // that has every other connection interleaved into it.
        using var scope = logger.BeginScope(new Dictionary<string, object>
        {
            ["SessionId"] = SessionId(connection),
            ["UserId"] = userId,
            ["Username"] = username,
        });

        registry.Add(connection);
        logger.LogDebug("Connection {ConnectionId} accepted for user {UserId}", connection.Id, userId);

        try
        {
            if (await HandshakeAsync(connection, reader, userId, username))
            {
                await PumpAsync(connection, reader);
            }
        }
        catch (Exception ex) when (ex is WebSocketException or OperationCanceledException)
        {
            logger.LogDebug(ex, "Connection {ConnectionId} (user {UserId}) torn down", connection.Id, userId);

            // The host stopping cancels this connection's lifetime, so the receive loop can
            // reach here before ConnectionRegistry.CloseAllAsync does. Both paths must close
            // with the same 1001 shutdown reason, whichever of them latches the close first.
            await (lifetime.ApplicationStopping.IsCancellationRequested
                ? CloseAsync(connection, GoingAwayClose, ShutdownReason)
                : CloseAsync(connection, GoingAwayClose, "connection lost"));
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Connection {ConnectionId} (user {UserId}) failed unexpectedly", connection.Id, userId);
            await CloseAsync(connection, WebSocketCloseStatus.InternalServerError, "internal error");
        }
        finally
        {
            // Detach first: it announces the leave while the connection is still the account's
            // live one, which Remove says nothing about.
            registry.Detach(connection);
            registry.Remove(connection);
            await reader.DrainAsync();
            connection.Dispose();
        }
    }

    private async Task<bool> HandshakeAsync(ClientConnection connection, SocketReader reader, long userId, string username)
    {
        var received = await reader.ReadAsync(HelloDeadline);
        switch (received.Outcome)
        {
            case ReceiveOutcome.ClientClose:
                await MirrorCloseAsync(connection, received);
                return false;
            case ReceiveOutcome.Closed:
                logger.LogDebug("Connection {ConnectionId} stopped receiving: close already latched", connection.Id);
                LogDisconnect(connection);
                return false;
            case ReceiveOutcome.Timeout:
                await FailAsync(connection, ErrorCode.Protocol, "no hello within 5s", ProtocolClose, "hello timeout");
                return false;
            case ReceiveOutcome.TooLarge:
                await FailAsync(connection, ErrorCode.Protocol, "message exceeds 16384 bytes", ProtocolClose, "message too large");
                return false;
            case ReceiveOutcome.TextFrame:
                await FailAsync(connection, ErrorCode.Protocol, "text frames are not accepted", ProtocolClose, "text frame");
                return false;
        }

        if (!TryParse(connection, received.Payload, out var frame))
        {
            await FailAsync(connection, ErrorCode.Protocol, "unparsable frame", ProtocolClose, "unparsable frame");
            return false;
        }

        if (frame.PayloadCase != ClientFrame.PayloadOneofCase.Hello)
        {
            await FailAsync(connection, ErrorCode.Protocol, "the first frame must be hello", ProtocolClose, "hello expected");
            return false;
        }

        if (frame.Hello.ProtocolVersion != SupportedProtocolVersion)
        {
            await FailAsync(connection, ErrorCode.Protocol, "unsupported protocol version", ProtocolClose, "unsupported version");
            return false;
        }

        var latestMessageId = await messages.GetLatestIdAsync();

        // Attach queues Welcome, the ServerSnapshot and the VoiceState frames under the registry
        // lock, so no delta can overtake them.
        var outcome = await registry.AttachAsync(connection, userId, username, latestMessageId, connection.Lifetime);
        if (!outcome.Attached)
        {
            await FailAsync(connection, ErrorCode.Forbidden, "not a member of this server", ProtocolClose, "not a member");
            return false;
        }

        if (outcome.Replaced is { } replaced)
        {
            // Detached and never awaited: the replaced socket may be dead, and this account's
            // new session must not wait out its close handshake.
            _ = FailQuietlyAsync(
                replaced,
                new Error { Code = ErrorCode.SessionReplaced, Detail = "this account connected from another device", Fatal = true },
                ProtocolClose,
                "session replaced");
        }

        var clientVersion = NormalizeClientField(frame.Hello.ClientVersion);
        var clientPlatform = NormalizeClientField(frame.Hello.ClientPlatform);
        await RecordClientAsync(userId, clientVersion, clientPlatform);

        logger.LogInformation(
            "Connection {ConnectionId} (session {SessionId}, user {UserId} {Username}, client {ClientVersion} {ClientPlatform}) connected; {ConnectionCount} live, latest message {LatestMessageId}",
            connection.Id,
            SessionId(connection),
            userId,
            username,
            clientVersion ?? "-",
            clientPlatform ?? "-",
            registry.Count,
            latestMessageId);
        return true;
    }

    // Hello carries these for the update endpoints and the admin CLI to report on; they are
    // whatever the client claims and are never enforced, so anything unusable becomes null.
    private static string? NormalizeClientField(string? raw)
    {
        var trimmed = raw?.Trim();
        if (string.IsNullOrEmpty(trimmed))
        {
            return null;
        }

        return trimmed.Length > MaxClientDescriptionLength ? trimmed[..MaxClientDescriptionLength] : trimmed;
    }

    // Best effort by design: the handshake is already complete, and losing a version reading is
    // never a reason to refuse the session.
    private async Task RecordClientAsync(long userId, string? clientVersion, string? clientPlatform)
    {
        try
        {
            await using var db = await contextFactory.CreateDbContextAsync();
            await db.Users
                .Where(u => u.Id == userId)
                .ExecuteUpdateAsync(setters => setters
                    .SetProperty(u => u.LastClientVersion, clientVersion)
                    .SetProperty(u => u.LastClientPlatform, clientPlatform)
                    .SetProperty(u => u.LastSeenAt, (DateTime?)DateTime.UtcNow));
        }
        catch (Exception ex)
        {
            logger.LogWarning(ex, "Could not record the client version of user {UserId}", userId);
        }
    }

    private async Task PumpAsync(ClientConnection connection, SocketReader reader)
    {
        // Per live connection, and an account only has one: a replaced session starts over with
        // a full bucket, which costs nothing a reconnect did not already cost.
        var limiter = new WriteLimiter(_limits);
        var rejectionLogged = false;

        while (true)
        {
            var received = await reader.ReadAsync(IdleDeadline);
            switch (received.Outcome)
            {
                case ReceiveOutcome.ClientClose:
                    await MirrorCloseAsync(connection, received);
                    return;
                case ReceiveOutcome.Closed:
                    logger.LogDebug(
                        "Connection {ConnectionId} ({Username}) stopped receiving: close already latched",
                        connection.Id,
                        connection.Username);
                    LogDisconnect(connection);
                    return;
                case ReceiveOutcome.Timeout:
                    await CloseAsync(connection, GoingAwayClose, "idle");
                    return;
                case ReceiveOutcome.TooLarge:
                    await FailAsync(connection, ErrorCode.Protocol, "message exceeds 16384 bytes", ProtocolClose, "message too large");
                    return;
                case ReceiveOutcome.TextFrame:
                    await FailAsync(connection, ErrorCode.Protocol, "text frames are not accepted", ProtocolClose, "text frame");
                    return;
            }

            if (!TryParse(connection, received.Payload, out var frame))
            {
                await FailAsync(connection, ErrorCode.Protocol, "unparsable frame", ProtocolClose, "unparsable frame");
                return;
            }

            // An account disabled while this socket was open is otherwise invisible to it: the
            // bearer was checked at the upgrade and never again. One cached lookup per write frame
            // closes that gap without the admin endpoint; reads stay free, like the limiter. A ban
            // is the registry's own business: it closes the socket as it writes the bans row.
            if (IsWrite(frame.PayloadCase)
                && connection.UserId is { } writer
                && await accounts.IsDisabledAsync(writer, connection.Lifetime))
            {
                logger.LogInformation(
                    "Connection {ConnectionId} of disabled account {UserId} closed at its next write frame",
                    connection.Id,
                    writer);
                await CloseAsync(connection, VorcallCloseStatus.Disabled, "account disabled");
                return;
            }

            // Only the five frames any member may send without a permission are charged; reads,
            // presence, voice, share signalling, pings and every permission-gated management or
            // moderation frame stay free so a throttled account can still navigate and a settings
            // page can save a burst of changes.
            if (IsRateLimited(frame.PayloadCase) && !limiter.TryTake())
            {
                Interlocked.Increment(ref WriteLimiter.RejectedTotal);
                if (!rejectionLogged)
                {
                    rejectionLogged = true;
                    logger.LogDebug(
                        "Connection {ConnectionId} hit the write rate limit on a {PayloadCase} frame",
                        connection.Id,
                        frame.PayloadCase);
                }

                if (!NonFatal(connection, ErrorCode.RateLimited, RateLimitedDetail))
                {
                    await CloseAsync(connection, VorcallCloseStatus.SlowConsumer, "slow consumer");
                    return;
                }

                continue;
            }

            switch (await DispatchAsync(connection, frame))
            {
                case Dispatch.SlowConsumer:
                    await CloseAsync(connection, VorcallCloseStatus.SlowConsumer, "slow consumer");
                    return;
                case Dispatch.DuplicateHello:
                    await FailAsync(connection, ErrorCode.Protocol, "hello was already received", ProtocolClose, "duplicate hello");
                    return;
                case Dispatch.EmptyPayload:
                    await FailAsync(connection, ErrorCode.Protocol, "frame carries no payload", ProtocolClose, "empty payload");
                    return;
            }
        }
    }

    // One case per payload of the oneof. Anything a client can get wrong is answered non-fatally
    // and the pump goes on; SlowConsumer means the sender's own outbox is full, which is the one
    // way a well-formed frame ends the connection.
    private async Task<Dispatch> DispatchAsync(ClientConnection connection, ClientFrame frame)
    {
        switch (frame.PayloadCase)
        {
            case ClientFrame.PayloadOneofCase.Hello:
                return Dispatch.DuplicateHello;

            case ClientFrame.PayloadOneofCase.Ping:
                return Flow(connection.TryEnqueue(
                    new ServerFrame { Pong = new Pong { SentAtUnixMs = frame.Ping.SentAtUnixMs } }));
        }

        if (!TryIdentify(connection, out var userId, out var username))
        {
            return Dispatch.Continue;
        }

        switch (frame.PayloadCase)
        {
            case ClientFrame.PayloadOneofCase.Send:
                return Flow(await HandleSendAsync(connection, userId, username, frame.Send));

            case ClientFrame.PayloadOneofCase.EditMessage:
                return Flow(await HandleEditAsync(connection, userId, frame.EditMessage));

            case ClientFrame.PayloadOneofCase.DeleteMessage:
                return Flow(await HandleDeleteAsync(connection, userId, frame.DeleteMessage));

            case ClientFrame.PayloadOneofCase.React:
                return Flow(await HandleReactAsync(connection, userId, frame.React));

            case ClientFrame.PayloadOneofCase.MarkRead:
                return Flow(await HandleMarkReadAsync(connection, userId, frame.MarkRead));

            case ClientFrame.PayloadOneofCase.OpenDm:
                return Flow(Answer(connection, await registry.OpenDmAsync(userId, frame.OpenDm.UserId, Persist)));

            case ClientFrame.PayloadOneofCase.JoinVoice:
                return Flow(HandleJoinVoice(connection, frame.JoinVoice));

            case ClientFrame.PayloadOneofCase.LeaveVoice:
                return Flow(HandleLeaveVoice(connection, frame.LeaveVoice));

            case ClientFrame.PayloadOneofCase.VoiceSelfState:
                return Flow(HandleVoiceSelfState(connection, frame.VoiceSelfState));

            case ClientFrame.PayloadOneofCase.StartShare:
                return Flow(HandleStartShare(connection, frame.StartShare));

            case ClientFrame.PayloadOneofCase.StopShare:
                return Flow(HandleStopShare(connection, frame.StopShare));

            case ClientFrame.PayloadOneofCase.WatchShare:
                return Flow(HandleWatchShare(connection, frame.WatchShare));

            case ClientFrame.PayloadOneofCase.UnwatchShare:
                return Flow(HandleUnwatchShare(connection, frame.UnwatchShare));

            case ClientFrame.PayloadOneofCase.CreateChannel:
                return Flow(await HandleCreateChannelAsync(connection, userId, frame.CreateChannel));

            case ClientFrame.PayloadOneofCase.UpdateChannel:
                return Flow(await HandleUpdateChannelAsync(connection, userId, frame.UpdateChannel));

            case ClientFrame.PayloadOneofCase.DeleteChannel:
                return Flow(Answer(
                    connection,
                    await registry.DeleteChannelAsync(userId, frame.DeleteChannel.Id, attachments, Persist)));

            case ClientFrame.PayloadOneofCase.CreateCategory:
                return Flow(await HandleCreateCategoryAsync(connection, userId, frame.CreateCategory));

            case ClientFrame.PayloadOneofCase.UpdateCategory:
                return Flow(await HandleUpdateCategoryAsync(connection, userId, frame.UpdateCategory));

            case ClientFrame.PayloadOneofCase.DeleteCategory:
                return Flow(Answer(connection, await registry.DeleteCategoryAsync(userId, frame.DeleteCategory.Id, Persist)));

            case ClientFrame.PayloadOneofCase.ReorderChannels:
                return Flow(await HandleReorderChannelsAsync(connection, userId, frame.ReorderChannels));

            case ClientFrame.PayloadOneofCase.ReorderCategories:
                return Flow(Answer(
                    connection,
                    await registry.ReorderCategoriesAsync(userId, frame.ReorderCategories.Ids.ToList(), Persist)));

            case ClientFrame.PayloadOneofCase.SetOverride:
                return Flow(await HandleSetOverrideAsync(connection, userId, frame.SetOverride));

            case ClientFrame.PayloadOneofCase.CreateRole:
                return Flow(await HandleCreateRoleAsync(connection, userId, frame.CreateRole));

            case ClientFrame.PayloadOneofCase.UpdateRole:
                return Flow(await HandleUpdateRoleAsync(connection, userId, frame.UpdateRole));

            case ClientFrame.PayloadOneofCase.DeleteRole:
                return Flow(Answer(connection, await registry.DeleteRoleAsync(userId, frame.DeleteRole.Id, Persist)));

            case ClientFrame.PayloadOneofCase.ReorderRoles:
                return Flow(Answer(
                    connection,
                    await registry.ReorderRolesAsync(userId, frame.ReorderRoles.Ids.ToList(), Persist)));

            case ClientFrame.PayloadOneofCase.SetMemberRoles:
                return Flow(Answer(
                    connection,
                    await registry.SetMemberRolesAsync(
                        userId,
                        frame.SetMemberRoles.UserId,
                        frame.SetMemberRoles.RoleIds.ToList(),
                        Persist)));

            case ClientFrame.PayloadOneofCase.SetNickname:
                return Flow(await HandleSetNicknameAsync(connection, userId, frame.SetNickname));

            case ClientFrame.PayloadOneofCase.KickMember:
                return Flow(Answer(connection, await registry.KickAsync(userId, frame.KickMember.UserId, Persist)));

            case ClientFrame.PayloadOneofCase.BanMember:
                return Flow(await HandleBanAsync(connection, userId, frame.BanMember));

            case ClientFrame.PayloadOneofCase.UnbanMember:
                return Flow(Answer(connection, await registry.UnbanAsync(userId, frame.UnbanMember.UserId, Persist)));

            case ClientFrame.PayloadOneofCase.UpdateServer:
                return Flow(await HandleUpdateServerAsync(connection, userId, frame.UpdateServer));

            case ClientFrame.PayloadOneofCase.TransferOwnership:
                return Flow(Answer(
                    connection,
                    await registry.TransferOwnershipAsync(userId, frame.TransferOwnership.UserId, Persist)));

            case ClientFrame.PayloadOneofCase.VoiceModerate:
                return Flow(await HandleVoiceModerateAsync(connection, userId, frame.VoiceModerate));

            case ClientFrame.PayloadOneofCase.UpdateProfile:
                return Flow(await HandleUpdateProfileAsync(connection, userId, frame.UpdateProfile));

            case ClientFrame.PayloadOneofCase.PlaySound:
                return Flow(HandlePlaySound(connection, frame.PlaySound));

            case ClientFrame.PayloadOneofCase.StopSound:
                return Flow(HandleStopSound(connection, frame.StopSound));

            case ClientFrame.PayloadOneofCase.UpdateSound:
                return Flow(await HandleUpdateSoundAsync(connection, frame.UpdateSound));

            case ClientFrame.PayloadOneofCase.DeleteSound:
                return Flow(Answer(connection, await registry.DeleteSoundAsync(connection, frame.DeleteSound.SoundId, Persist)));

            default:
                return Dispatch.EmptyPayload;
        }
    }

    private async Task<bool> HandleSendAsync(ClientConnection connection, long userId, string username, SendMessage send)
    {
        if (!Validation.TryParseChannelId(send.ChannelId, out var channelId)
            || registry.ChannelOf(channelId) is not { } channel
            || !registry.CanView(userId, channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        if (channel.Kind == Data.ChannelKind.Voice)
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "channel");
        }

        var attachmentIds = send.AttachmentIds.ToList();
        var streamedFileIds = send.StreamedFileIds.ToList();
        var carriesFiles = attachmentIds.Count > 0 || streamedFileIds.Count > 0;

        // Text may be empty, and only then, when the message carries an attachment or a streamed
        // file instead.
        if (!Validation.TryNormalizeText(send.Text, out var text)
            && !(carriesFiles && string.IsNullOrWhiteSpace(send.Text)))
        {
            return NonFatal(connection, ErrorCode.InvalidMessage, InvalidTextDetail);
        }

        if (!registry.Has(userId, channelId, Perm.SendMessages))
        {
            return Denied(connection, Perm.SendMessages);
        }

        // A streamed file is attached like any other; the same bit covers both.
        if (carriesFiles && !registry.Has(userId, channelId, Perm.AttachFiles))
        {
            return Denied(connection, Perm.AttachFiles);
        }

        // The count and the duplicates are decided here so a frame that can never be accepted
        // never opens a transaction; whose the files are is the append's own business.
        if (attachmentIds.Count > AttachmentsOptions.MaxPerMessage
            || attachmentIds.Distinct().Count() != attachmentIds.Count)
        {
            return NonFatal(connection, ErrorCode.InvalidAttachment, InvalidAttachmentDetail);
        }

        if (streamedFileIds.Count > StreamOptions.MaxPerMessage
            || streamedFileIds.Distinct().Count() != streamedFileIds.Count)
        {
            return NonFatal(connection, ErrorCode.InvalidStream, InvalidStreamDetail);
        }

        // Persist first: an id only exists once the row is committed, and the broadcast
        // carries that id.
        var outcome = await messages.AppendAsync(
            userId,
            username,
            channelId,
            text,
            send.ReplyToId,
            attachmentIds,
            streamedFileIds,
            registry.Has(userId, channelId, Perm.MentionEveryone));
        switch (outcome.Status)
        {
            case AppendOutcome.Kind.UnknownReply:
                return NonFatal(connection, ErrorCode.UnknownMessage, "reply target is not in this channel");

            case AppendOutcome.Kind.InvalidAttachment:
                return NonFatal(connection, ErrorCode.InvalidAttachment, InvalidAttachmentDetail);

            case AppendOutcome.Kind.InvalidStream:
                return NonFatal(connection, ErrorCode.InvalidStream, InvalidStreamDetail);
        }

        registry.BroadcastToChannel(channelId, new ServerFrame { Message = outcome.Message! });
        return true;
    }

    private async Task<bool> HandleEditAsync(ClientConnection connection, long userId, EditMessage edit)
    {
        if (!Validation.TryNormalizeText(edit.Text, out var text))
        {
            return NonFatal(connection, ErrorCode.InvalidMessage, InvalidTextDetail);
        }

        // The channel is read first: whether the caller may touch the message at all is decided
        // before anything is written.
        if (await messages.ChannelOfAsync(edit.Id) is not { } channelId)
        {
            return NonFatal(connection, ErrorCode.UnknownMessage, UnknownMessageDetail);
        }

        if (!registry.CanView(userId, channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        var outcome = await messages.EditAsync(edit.Id, userId, text, registry.Has(userId, channelId, Perm.MentionEveryone));
        switch (outcome.Status)
        {
            case EditOutcome.Kind.Unknown:
                return NonFatal(connection, ErrorCode.UnknownMessage, UnknownMessageDetail);

            // MANAGE_MESSAGES does not grant editing someone else's text.
            case EditOutcome.Kind.Forbidden:
                return NonFatal(connection, ErrorCode.Forbidden, "only the author can edit a message");
        }

        registry.BroadcastToChannel(
            outcome.ChannelId,
            new ServerFrame { MessageEdited = new MessageEdited { Message = outcome.Message! } });
        return true;
    }

    private async Task<bool> HandleDeleteAsync(ClientConnection connection, long userId, DeleteMessage delete)
    {
        if (await messages.ChannelOfAsync(delete.Id) is not { } channelId)
        {
            return NonFatal(connection, ErrorCode.UnknownMessage, UnknownMessageDetail);
        }

        if (!registry.CanView(userId, channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        var outcome = await messages.DeleteAsync(delete.Id, userId, registry.Has(userId, channelId, Perm.ManageMessages));
        switch (outcome.Status)
        {
            case DeleteOutcome.Kind.Unknown:
                return NonFatal(connection, ErrorCode.UnknownMessage, UnknownMessageDetail);

            // Someone else's message without the bit that would let it go: PROTOCOL.md § Messages
            // names the missing permission rather than answering FORBIDDEN.
            case DeleteOutcome.Kind.Forbidden:
                return Denied(connection, Perm.ManageMessages);
        }

        logger.LogInformation(
            "User {UserId} deleted message {MessageId} in channel {ChannelId}",
            userId,
            delete.Id,
            outcome.ChannelId);
        registry.BroadcastToChannel(
            outcome.ChannelId,
            new ServerFrame { MessageDeleted = new MessageDeleted { ChannelId = outcome.ChannelId, Id = delete.Id } });

        // The rows are already gone; the files follow them.
        attachments.DeleteFiles(outcome.Attachments);
        return true;
    }

    private async Task<bool> HandleReactAsync(ClientConnection connection, long userId, React react)
    {
        if (!AcceptedReactions.Contains(react.Emoji))
        {
            return NonFatal(connection, ErrorCode.InvalidReaction, "emoji is not one of the accepted reactions");
        }

        if (await messages.ChannelOfAsync(react.MessageId) is not { } channelId)
        {
            return NonFatal(connection, ErrorCode.UnknownMessage, UnknownMessageDetail);
        }

        if (!registry.CanView(userId, channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        // Removing one needs the bit too.
        if (!registry.Has(userId, channelId, Perm.AddReactions))
        {
            return Denied(connection, Perm.AddReactions);
        }

        var outcome = await messages.ReactAsync(react.MessageId, userId, react.Emoji, react.Remove);
        if (outcome.Status is ReactOutcome.Kind.Unknown)
        {
            return NonFatal(connection, ErrorCode.UnknownMessage, UnknownMessageDetail);
        }

        // The full grouped set, so the broadcast replaces what every client knew.
        var changed = new ReactionsChanged { ChannelId = outcome.ChannelId, MessageId = react.MessageId };
        changed.Reactions.AddRange(outcome.Reactions);
        registry.BroadcastToChannel(outcome.ChannelId, new ServerFrame { ReactionsChanged = changed });
        return true;
    }

    private async Task<bool> HandleMarkReadAsync(ClientConnection connection, long userId, MarkRead mark)
    {
        if (!Validation.TryParseChannelId(mark.ChannelId, out var channelId)
            || registry.ChannelOf(channelId) is not { } channel
            || !registry.CanView(userId, channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        // A voice channel holds no messages, so it has no cursor either.
        if (channel.Kind == Data.ChannelKind.Voice)
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "channel");
        }

        // The cursor only ever moves forward, and the frame has no answer.
        await channels.MarkReadAsync(channelId, userId, mark.MessageId, Persist);
        return true;
    }

    private bool HandleJoinVoice(ClientConnection connection, JoinVoice join)
    {
        if (!Validation.TryParseChannelId(join.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.JoinVoice(connection, channelId, join.SelfMuted, join.SelfDeafened))
        {
            case JoinVoiceOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case JoinVoiceOutcome.NotVoiceChannel:
                return NonFatal(connection, ErrorCode.InvalidArgument, "channel");

            case JoinVoiceOutcome.PermissionDenied:
                return Denied(connection, Perm.Connect);

            case JoinVoiceOutcome.Unavailable:
                return NonFatal(connection, ErrorCode.VoiceUnavailable, "voice is not available on this server");

            case JoinVoiceOutcome.NotLive:
                return Dropped(connection, "joined voice");

            // Joined: the registry has already queued VoiceReady and VoiceState.
            default:
                return true;
        }
    }

    private bool HandleLeaveVoice(ClientConnection connection, LeaveVoice leave)
    {
        if (!Validation.TryParseChannelId(leave.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.LeaveVoice(connection, channelId))
        {
            case LeaveVoiceOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case LeaveVoiceOutcome.NotInVoice:
                return NonFatal(connection, ErrorCode.NotInVoice, "not in that channel's voice session");

            case LeaveVoiceOutcome.NotLive:
                return Dropped(connection, "left voice");

            default:
                return true;
        }
    }

    private bool HandleVoiceSelfState(ClientConnection connection, VoiceSelfState self)
    {
        if (!Validation.TryParseChannelId(self.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.SetVoiceSelfState(connection, channelId, self.Muted, self.Deafened))
        {
            case VoiceSelfStateOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case VoiceSelfStateOutcome.NotInVoice:
                return NonFatal(connection, ErrorCode.NotInVoice, NotInVoiceDetail);

            case VoiceSelfStateOutcome.NotLive:
                return Dropped(connection, "set its own voice state");

            // Unchanged and Set are both silent: the registry has already broadcast whatever moved.
            default:
                return true;
        }
    }

    private bool HandleStartShare(ClientConnection connection, StartShare start)
    {
        if (!Validation.TryParseChannelId(start.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.StartShare(connection, channelId, start.Audio))
        {
            case StartShareOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case StartShareOutcome.NotInVoice:
                return NonFatal(connection, ErrorCode.NotInVoice, NotInVoiceDetail);

            case StartShareOutcome.PermissionDenied:
                return Denied(connection, Perm.ShareScreen);

            case StartShareOutcome.Unavailable:
                return NonFatal(connection, ErrorCode.ShareUnavailable, "screen share is disabled on this server");

            case StartShareOutcome.Limit:
                return NonFatal(connection, ErrorCode.ShareLimit, "this channel already has the maximum number of sharers");

            case StartShareOutcome.NotLive:
                return Dropped(connection, "started a share");

            // Started: the registry has already queued ShareStarted and ShareWatchers.
            default:
                return true;
        }
    }

    private bool HandleStopShare(ClientConnection connection, StopShare stop)
    {
        if (!Validation.TryParseChannelId(stop.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.StopShare(connection, channelId))
        {
            case StopShareOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case StopShareOutcome.NotInVoice:
                return NonFatal(connection, ErrorCode.NotInVoice, NotInVoiceDetail);

            case StopShareOutcome.NotSharing:
                return NonFatal(connection, ErrorCode.NotSharing, "not sharing");

            case StopShareOutcome.NotLive:
                return Dropped(connection, "stopped a share");

            default:
                return true;
        }
    }

    private bool HandleWatchShare(ClientConnection connection, WatchShare watch)
    {
        if (!Validation.TryParseChannelId(watch.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.WatchShare(connection, channelId, watch.UserId))
        {
            case WatchShareOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case WatchShareOutcome.NotInVoice:
                return NonFatal(connection, ErrorCode.NotInVoice, NotInVoiceDetail);

            case WatchShareOutcome.NotSharing:
                return NonFatal(connection, ErrorCode.NotSharing, "that user is not sharing");

            case WatchShareOutcome.NotLive:
                return Dropped(connection, "watched a share");

            // Watching: the registry has already queued WatchState and the sharers' ShareWatchers.
            default:
                return true;
        }
    }

    private bool HandleUnwatchShare(ClientConnection connection, UnwatchShare unwatch)
    {
        if (!Validation.TryParseChannelId(unwatch.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        switch (registry.UnwatchShare(connection, channelId))
        {
            case UnwatchShareOutcome.UnknownChannel:
                return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);

            case UnwatchShareOutcome.NotInVoice:
                return NonFatal(connection, ErrorCode.NotInVoice, NotInVoiceDetail);

            case UnwatchShareOutcome.NotLive:
                return Dropped(connection, "unwatched a share");

            default:
                return true;
        }
    }

    private bool HandlePlaySound(ClientConnection connection, PlaySound play)
    {
        if (!Validation.TryParseChannelId(play.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        return Answer(connection, registry.PlaySound(connection, channelId, play.SoundId));
    }

    private bool HandleStopSound(ClientConnection connection, StopSound stop)
    {
        if (!Validation.TryParseChannelId(stop.ChannelId, out var channelId))
        {
            return NonFatal(connection, ErrorCode.UnknownChannel, UnknownChannelDetail);
        }

        return Answer(connection, registry.StopSound(connection, channelId));
    }

    private async Task<bool> HandleUpdateSoundAsync(ClientConnection connection, UpdateSound update)
    {
        if (!Names.TryNormalize(update.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "name");
        }

        return Answer(connection, await registry.UpdateSoundAsync(connection, update.SoundId, name, Persist));
    }

    private async Task<bool> HandleCreateChannelAsync(ClientConnection connection, long userId, CreateChannel create)
    {
        // A DM is opened with OpenDm, never created here.
        var wanted = create.Kind switch
        {
            Protocol.ChannelKind.Text => Data.ChannelKind.Text,
            Protocol.ChannelKind.Voice => Data.ChannelKind.Voice,
            _ => (Data.ChannelKind?)null,
        };
        if (wanted is not { } kind)
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "kind");
        }

        if (!Names.TryNormalize(create.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        if (!Names.TryNormalizeLong(create.Topic, out var topic))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "topic");
        }

        return Answer(connection, await registry.CreateChannelAsync(userId, kind, name, topic, create.CategoryId, Persist));
    }

    private async Task<bool> HandleUpdateChannelAsync(ClientConnection connection, long userId, UpdateChannel update)
    {
        if (!Names.TryNormalize(update.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        if (!Names.TryNormalizeLong(update.Topic, out var topic))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "topic");
        }

        return Answer(connection, await registry.UpdateChannelAsync(userId, update.Id, name, topic, Persist));
    }

    private async Task<bool> HandleCreateCategoryAsync(ClientConnection connection, long userId, CreateCategory create)
    {
        if (!Names.TryNormalize(create.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        return Answer(connection, await registry.CreateCategoryAsync(userId, name, Persist));
    }

    private async Task<bool> HandleUpdateCategoryAsync(ClientConnection connection, long userId, UpdateCategory update)
    {
        if (!Names.TryNormalize(update.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        return Answer(connection, await registry.UpdateCategoryAsync(userId, update.Id, name, Persist));
    }

    private async Task<bool> HandleReorderChannelsAsync(ClientConnection connection, long userId, ReorderChannels reorder)
    {
        var positions = reorder.Positions
            .Select(position => (position.Id, position.CategoryId, position.Position))
            .ToList();
        return Answer(connection, await registry.ReorderChannelsAsync(userId, positions, Persist));
    }

    private async Task<bool> HandleSetOverrideAsync(ClientConnection connection, long userId, SetOverride set)
    {
        if (set.Override is not { } wanted)
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "override");
        }

        return Answer(
            connection,
            await registry.SetOverrideAsync(
                userId,
                set.ChannelId,
                wanted.RoleId,
                wanted.UserId,
                wanted.Allow,
                wanted.Deny,
                Persist));
    }

    private async Task<bool> HandleCreateRoleAsync(ClientConnection connection, long userId, CreateRole create)
    {
        if (!Names.TryNormalize(create.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        if (!Names.TryNormalizeEmoji(create.IconEmoji, out var emoji))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "icon_emoji");
        }

        return Answer(
            connection,
            await registry.CreateRoleAsync(
                userId,
                name,
                create.Color,
                emoji,
                create.IconImageId,
                create.Permissions,
                create.Hoist,
                images,
                Persist));
    }

    private async Task<bool> HandleUpdateRoleAsync(ClientConnection connection, long userId, UpdateRole update)
    {
        if (update.Role is not { } role)
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "role");
        }

        if (!Names.TryNormalize(role.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        if (!Names.TryNormalizeEmoji(role.IconEmoji, out var emoji))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "icon_emoji");
        }

        // A copy: the registry compares the normalised text against the stored row, and the frame
        // this handler was handed stays exactly what the client sent.
        var normalized = role.Clone();
        normalized.Name = name;
        normalized.IconEmoji = emoji;
        return Answer(connection, await registry.UpdateRoleAsync(userId, normalized, images, Persist));
    }

    private async Task<bool> HandleSetNicknameAsync(ClientConnection connection, long userId, SetNickname set)
    {
        // 0 is the wire's "self"; the registry is never handed it.
        var target = set.UserId == 0 ? userId : set.UserId;

        string nickname;
        if (string.IsNullOrWhiteSpace(set.Nickname))
        {
            nickname = string.Empty;
        }
        else if (!Names.TryNormalize(set.Nickname, out nickname))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        return Answer(connection, await registry.SetNicknameAsync(userId, target, nickname, Persist));
    }

    private async Task<bool> HandleBanAsync(ClientConnection connection, long userId, BanMember ban)
    {
        if (!Names.TryNormalizeLong(ban.Reason, out var reason))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "reason");
        }

        return Answer(connection, await registry.BanAsync(userId, ban.UserId, reason, attachments, Persist));
    }

    private async Task<bool> HandleUpdateServerAsync(ClientConnection connection, long userId, UpdateServer update)
    {
        if (!Names.TryNormalize(update.Name, out var name))
        {
            return NonFatal(connection, ErrorCode.InvalidName, InvalidNameDetail);
        }

        if (!Names.TryNormalizeLong(update.Description, out var description))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "description");
        }

        return Answer(
            connection,
            await registry.UpdateServerAsync(userId, name, description, update.IconImageId, images, Persist));
    }

    // Each flag applies only when its set_* companion is true, and a move_to of 0 disconnects
    // instead of moving; null is "leave this one alone".
    private async Task<bool> HandleVoiceModerateAsync(ClientConnection connection, long userId, VoiceModerate moderate)
        => Answer(
            connection,
            await registry.VoiceModerateAsync(
                userId,
                moderate.UserId,
                moderate.ChannelId,
                moderate.SetMuted ? moderate.Muted : (bool?)null,
                moderate.SetDeafened ? moderate.Deafened : (bool?)null,
                moderate.Move ? moderate.MoveTo : (long?)null,
                Persist));

    private async Task<bool> HandleUpdateProfileAsync(ClientConnection connection, long userId, UpdateProfile update)
    {
        if (!Names.TryNormalizeLong(update.Description, out var description))
        {
            return NonFatal(connection, ErrorCode.InvalidArgument, "description");
        }

        // The two image ids keep their wire sentinels: 0 keeps the current image, -1 clears it.
        return Answer(
            connection,
            await registry.UpdateProfileAsync(
                userId,
                description,
                update.AccentColor,
                update.AvatarImageId,
                update.BannerImageId,
                images,
                Persist));
    }

    // A latched close must never persist or broadcast anything, however the frame was read: the
    // close wins even if it landed mid-receive. MarkReady sets the identity before IsReady, so a
    // ready connection always has one.
    private bool TryIdentify(ClientConnection connection, out long userId, out string username)
    {
        if (connection.IsReady && connection.UserId is { } identified && connection.Username is { } name)
        {
            userId = identified;
            username = name;
            return true;
        }

        logger.LogDebug("Connection {ConnectionId} acted after close was latched; dropping", connection.Id);
        userId = 0;
        username = string.Empty;
        return false;
    }

    // Turns one operation's verdict into the answer PROTOCOL.md owes the caller; the operation has
    // already broadcast whatever its success means. NotLive answers nothing at all: the account has
    // replaced this connection.
    private bool Answer(ClientConnection connection, OpResult result)
    {
        if (result.IsOk)
        {
            return true;
        }

        if (result.Status is OpStatus.NotLive)
        {
            return Dropped(connection, "ran an operation");
        }

        return NonFatal(connection, Map(result.Status), result.Detail);
    }

    private static ErrorCode Map(OpStatus status) => status switch
    {
        OpStatus.PermissionDenied => ErrorCode.PermissionDenied,
        OpStatus.Hierarchy => ErrorCode.Hierarchy,
        OpStatus.Forbidden => ErrorCode.Forbidden,
        OpStatus.InvalidArgument => ErrorCode.InvalidArgument,
        OpStatus.UnknownChannel => ErrorCode.UnknownChannel,
        OpStatus.UnknownCategory => ErrorCode.UnknownCategory,
        OpStatus.UnknownRole => ErrorCode.UnknownRole,
        OpStatus.UnknownUser => ErrorCode.UnknownUser,
        OpStatus.UnknownImage => ErrorCode.UnknownImage,
        OpStatus.UnknownSound => ErrorCode.UnknownSound,
        OpStatus.NotInVoice => ErrorCode.NotInVoice,
        _ => throw new ArgumentOutOfRangeException(nameof(status), status, "this verdict is answered before it is mapped"),
    };

    // The detail of a PERMISSION_DENIED is the missing bit's name, which is part of the wire
    // contract (PROTOCOL.md § Roles and permissions).
    private static bool Denied(ClientConnection connection, Perm bit)
        => NonFatal(connection, ErrorCode.PermissionDenied, PermNames.Name(bit));

    // Every frame that persists a row, which is every frame the registry or MessageService writes
    // for. MarkRead is left out although it moves a cursor: the client debounces it to one call a
    // second per channel, and charging navigation is what the limiter is meant to avoid. Hello,
    // Ping, the voice and share signalling frames write nothing at all.
    private static bool IsWrite(ClientFrame.PayloadOneofCase payloadCase) => payloadCase is
        ClientFrame.PayloadOneofCase.Send
        or ClientFrame.PayloadOneofCase.EditMessage
        or ClientFrame.PayloadOneofCase.DeleteMessage
        or ClientFrame.PayloadOneofCase.React
        or ClientFrame.PayloadOneofCase.OpenDm
        or ClientFrame.PayloadOneofCase.CreateChannel
        or ClientFrame.PayloadOneofCase.UpdateChannel
        or ClientFrame.PayloadOneofCase.DeleteChannel
        or ClientFrame.PayloadOneofCase.CreateCategory
        or ClientFrame.PayloadOneofCase.UpdateCategory
        or ClientFrame.PayloadOneofCase.DeleteCategory
        or ClientFrame.PayloadOneofCase.ReorderChannels
        or ClientFrame.PayloadOneofCase.ReorderCategories
        or ClientFrame.PayloadOneofCase.SetOverride
        or ClientFrame.PayloadOneofCase.CreateRole
        or ClientFrame.PayloadOneofCase.UpdateRole
        or ClientFrame.PayloadOneofCase.DeleteRole
        or ClientFrame.PayloadOneofCase.ReorderRoles
        or ClientFrame.PayloadOneofCase.SetMemberRoles
        or ClientFrame.PayloadOneofCase.SetNickname
        or ClientFrame.PayloadOneofCase.KickMember
        or ClientFrame.PayloadOneofCase.BanMember
        or ClientFrame.PayloadOneofCase.UnbanMember
        or ClientFrame.PayloadOneofCase.UpdateServer
        or ClientFrame.PayloadOneofCase.TransferOwnership
        or ClientFrame.PayloadOneofCase.VoiceModerate
        or ClientFrame.PayloadOneofCase.UpdateProfile
        or ClientFrame.PayloadOneofCase.PlaySound
        or ClientFrame.PayloadOneofCase.StopSound
        or ClientFrame.PayloadOneofCase.UpdateSound
        or ClientFrame.PayloadOneofCase.DeleteSound;

    // Every frame an unprivileged account can send at will, so they are what a flood would
    // actually come from; the write limiter charges only them. Most management and moderation
    // frames in IsWrite above sit behind a permission bit (MANAGE_CHANNELS, MANAGE_ROLES,
    // BAN_MEMBERS, ...) that only a trusted member holds, and a settings page legitimately fires
    // many of them in one burst — e.g. one SetOverride per switch while editing a role's
    // permissions. The four soundpad frames are charged whatever bit they sit behind: this bucket
    // is the only spam control the soundpad has, SOUNDPAD being an @everyone default.
    private static bool IsRateLimited(ClientFrame.PayloadOneofCase payloadCase) => payloadCase is
        ClientFrame.PayloadOneofCase.Send
        or ClientFrame.PayloadOneofCase.EditMessage
        or ClientFrame.PayloadOneofCase.DeleteMessage
        or ClientFrame.PayloadOneofCase.React
        or ClientFrame.PayloadOneofCase.OpenDm
        or ClientFrame.PayloadOneofCase.PlaySound
        or ClientFrame.PayloadOneofCase.StopSound
        or ClientFrame.PayloadOneofCase.UpdateSound
        or ClientFrame.PayloadOneofCase.DeleteSound;

    // Non-fatal errors ride the same outbox as everything else, so a refusal means the sender
    // itself has fallen behind and the caller closes it as a slow consumer.
    private static bool NonFatal(ClientConnection connection, ErrorCode code, string detail)
        => connection.TryEnqueue(new ServerFrame { Error = new Error { Code = code, Detail = detail, Fatal = false } });

    // A connection the account has already replaced: its frames are answered with nothing at all.
    private bool Dropped(ClientConnection connection, string action)
    {
        logger.LogDebug("Connection {ConnectionId} {Action} after being replaced; dropping", connection.Id, action);
        return true;
    }

    private static Dispatch Flow(bool alive) => alive ? Dispatch.Continue : Dispatch.SlowConsumer;

    // A pending WebSocket receive cannot be cancelled without aborting the socket, which would
    // destroy the connection before the fatal error and close frames could be written. Deadlines
    // are therefore enforced by racing the receive against a timer and leaving it pending; the
    // same pending receive later picks up the peer's answer to our close frame.
    private sealed class SocketReader(ClientConnection connection, ILogger logger)
    {
        private static readonly TimeSpan DrainTimeout = TimeSpan.FromSeconds(2);

        private readonly byte[] _buffer = new byte[ReceiveBufferSize];
        private Task<WebSocketReceiveResult>? _pending;

        public async Task<ReceiveResult> ReadAsync(TimeSpan deadline)
        {
            var deadlineAt = Environment.TickCount64 + (long)deadline.TotalMilliseconds;
            using var assembled = new MemoryStream();

            while (true)
            {
                var receive = _pending ??= connection.Socket.ReceiveAsync(new ArraySegment<byte>(_buffer), CancellationToken.None);
                if (!await CompletesInTimeAsync(receive, deadlineAt))
                {
                    connection.Lifetime.ThrowIfCancellationRequested();
                    if (connection.Closing.IsCancellationRequested)
                    {
                        return ReceiveResult.Signal(ReceiveOutcome.Closed);
                    }

                    return ReceiveResult.Signal(ReceiveOutcome.Timeout);
                }

                _pending = null;
                var result = await receive;

                switch (result.MessageType)
                {
                    case WebSocketMessageType.Close:
                        return ReceiveResult.ClientClose(
                            result.CloseStatus ?? WebSocketCloseStatus.NormalClosure,
                            result.CloseStatusDescription ?? string.Empty);
                    case WebSocketMessageType.Text:
                        return ReceiveResult.Signal(ReceiveOutcome.TextFrame);
                }

                if (assembled.Length + result.Count > MaxInboundBytes)
                {
                    return ReceiveResult.Signal(ReceiveOutcome.TooLarge);
                }

                assembled.Write(_buffer, 0, result.Count);
                if (result.EndOfMessage)
                {
                    return ReceiveResult.Frame(assembled.ToArray());
                }
            }
        }

        // Observes the receive left pending by a deadline, so it is never an unobserved task.
        // A peer that answered our close frame completes it at once; one that went quiet is
        // cut off instead of holding the request open.
        public async Task DrainAsync()
        {
            var pending = _pending;
            if (pending is null)
            {
                return;
            }

            _pending = null;
            try
            {
                if (await Task.WhenAny(pending, Task.Delay(DrainTimeout)) != pending)
                {
                    connection.Socket.Abort();
                }

                await pending;
            }
            catch (Exception ex) when (ex is WebSocketException or OperationCanceledException or ObjectDisposedException)
            {
                logger.LogDebug(ex, "Connection {ConnectionId}: pending receive ended", connection.Id);
            }
            catch (Exception ex)
            {
                logger.LogError(ex, "Connection {ConnectionId}: pending receive failed unexpectedly", connection.Id);
            }
        }

        private async Task<bool> CompletesInTimeAsync(Task receive, long deadlineAt)
        {
            var remaining = deadlineAt - Environment.TickCount64;
            if (remaining <= 0)
            {
                return false;
            }

            // The timer is cancelled as soon as the receive wins so it does not linger.
            using var timer = CancellationTokenSource.CreateLinkedTokenSource(connection.Lifetime, connection.Closing);
            var finished = await Task.WhenAny(receive, Task.Delay(TimeSpan.FromMilliseconds(remaining), timer.Token));
            timer.Cancel();
            return finished == receive;
        }
    }

    private bool TryParse(ClientConnection connection, byte[] payload, out ClientFrame frame)
    {
        try
        {
            frame = ClientFrame.Parser.ParseFrom(payload);
            return true;
        }
        catch (InvalidProtocolBufferException ex)
        {
            logger.LogWarning(ex, "Connection {ConnectionId} ({Username}) sent an unparsable frame", connection.Id, connection.Username);
            frame = new ClientFrame();
            return false;
        }
    }

    private async Task MirrorCloseAsync(ClientConnection connection, ReceiveResult received)
        => await CloseAsync(connection, received.ClientStatus, received.ClientReason);

    private async Task FailAsync(ClientConnection connection, ErrorCode code, string detail, WebSocketCloseStatus status, string reason)
    {
        logger.LogWarning(
            "Connection {ConnectionId} ({Username}) protocol error {ErrorCode}: {Detail}",
            connection.Id,
            connection.Username,
            code,
            detail);
        await connection.FailAsync(new Error { Code = code, Detail = detail, Fatal = true }, status, reason);
        LogDisconnect(connection, status, reason);
    }

    // For a connection this handler does not own: it is being torn down from another
    // connection's handshake, so its failures belong in the log and nowhere else.
    private async Task FailQuietlyAsync(ClientConnection connection, Error error, WebSocketCloseStatus status, string reason)
    {
        try
        {
            await connection.FailAsync(error, status, reason);
            LogDisconnect(connection, status, reason);
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Connection {ConnectionId}: failed to close with {CloseCode}", connection.Id, (int)status);
        }
    }

    private async Task CloseAsync(ClientConnection connection, WebSocketCloseStatus status, string reason)
    {
        await connection.CloseAsync(status, reason);
        LogDisconnect(connection, status, reason);
    }

    // The latched pair, not the requested one: CloseAsync is idempotent, so an earlier caller
    // (a 1013 from the registry, say) may already have decided how this connection ends. The
    // requested pair is only a fallback, and a close this handler awaited always latched one.
    private void LogDisconnect(ClientConnection connection, WebSocketCloseStatus? requestedStatus = null, string? requestedReason = null)
        => logger.LogInformation(
            "Connection {ConnectionId} (session {SessionId}, {Username}) disconnected with {CloseCode} {CloseReason}",
            connection.Id,
            SessionId(connection),
            connection.Username,
            (int)(connection.CloseStatus ?? requestedStatus ?? WebSocketCloseStatus.Empty),
            connection.CloseReason ?? requestedReason ?? string.Empty);

    // The first eight hex digits of the connection id: short enough to head every console line,
    // still unique among the handful of sessions a friend group ever has open at once.
    private static string SessionId(ClientConnection connection) => connection.Id.ToString("N")[..8];

    // What the pump does after one frame: carry on, or end the connection the way the frame asked
    // for.
    private enum Dispatch
    {
        Continue,
        SlowConsumer,
        DuplicateHello,
        EmptyPayload,
    }

    private enum ReceiveOutcome
    {
        Frame,
        ClientClose,
        Closed,
        Timeout,
        TooLarge,
        TextFrame,
    }

    private readonly record struct ReceiveResult(
        ReceiveOutcome Outcome,
        byte[] Payload,
        WebSocketCloseStatus ClientStatus,
        string ClientReason)
    {
        public static ReceiveResult Signal(ReceiveOutcome outcome) => new(outcome, [], WebSocketCloseStatus.Empty, string.Empty);

        public static ReceiveResult Frame(byte[] payload) => new(ReceiveOutcome.Frame, payload, WebSocketCloseStatus.Empty, string.Empty);

        public static ReceiveResult ClientClose(WebSocketCloseStatus status, string reason) => new(ReceiveOutcome.ClientClose, [], status, reason);
    }
}
