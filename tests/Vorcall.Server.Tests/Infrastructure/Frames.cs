using Vorcall.Server.Protocol;

namespace Vorcall.Server.Tests.Infrastructure;

// One builder per ClientFrame payload.
internal static class Frames
{
    public static ClientFrame Hello(uint protocolVersion = 1, string clientVersion = "", string clientPlatform = "")
        => new() { Hello = new Hello { ProtocolVersion = protocolVersion, ClientVersion = clientVersion, ClientPlatform = clientPlatform } };

    public static ClientFrame Send(string text, string roomId = "", long replyToId = 0, params long[] attachmentIds)
    {
        var send = new SendMessage { Text = text, RoomId = roomId, ReplyToId = replyToId };
        send.AttachmentIds.AddRange(attachmentIds);
        return new ClientFrame { Send = send };
    }

    public static ClientFrame Ping(long sentAtUnixMs) => new() { Ping = new Ping { SentAtUnixMs = sentAtUnixMs } };

    public static ClientFrame Join(string roomId) => new() { JoinRoom = new JoinRoom { RoomId = roomId } };

    public static ClientFrame Leave(string roomId) => new() { LeaveRoom = new LeaveRoom { RoomId = roomId } };

    public static ClientFrame CreateRoom(string name) => new() { CreateRoom = new CreateRoom { Name = name } };

    public static ClientFrame OpenDm(long userId) => new() { OpenDm = new OpenDm { UserId = userId } };

    public static ClientFrame MarkRead(string roomId, long messageId)
        => new() { MarkRead = new MarkRead { RoomId = roomId, MessageId = messageId } };

    public static ClientFrame Edit(long messageId, string text) => new() { EditMessage = new EditMessage { Id = messageId, Text = text } };

    public static ClientFrame Delete(long messageId) => new() { DeleteMessage = new DeleteMessage { Id = messageId } };

    public static ClientFrame React(long messageId, string emoji, bool remove = false)
        => new() { React = new React { MessageId = messageId, Emoji = emoji, Remove = remove } };

    public static ClientFrame JoinVoice(string roomId) => new() { JoinVoice = new JoinVoice { RoomId = roomId } };

    public static ClientFrame LeaveVoice(string roomId) => new() { LeaveVoice = new LeaveVoice { RoomId = roomId } };

    public static ClientFrame StartShare(string roomId, bool audio = false)
        => new() { StartShare = new StartShare { RoomId = roomId, Audio = audio } };

    public static ClientFrame StopShare(string roomId) => new() { StopShare = new StopShare { RoomId = roomId } };

    public static ClientFrame WatchShare(string roomId, long userId)
        => new() { WatchShare = new WatchShare { RoomId = roomId, UserId = userId } };

    public static ClientFrame UnwatchShare(string roomId) => new() { UnwatchShare = new UnwatchShare { RoomId = roomId } };

    public static long NowMs() => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
}
