using Vorcall.Server.Protocol;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests.Infrastructure;

// Several fresh accounts, connected in order and all in general, with every MemberJoined the
// arrivals caused already consumed: the starting point of most room-level tests.
internal sealed class Party : IAsyncDisposable
{
    private readonly List<Account> _accounts;
    private readonly List<WsClient> _clients;

    private Party(List<Account> accounts, List<WsClient> clients)
    {
        _accounts = accounts;
        _clients = clients;
    }

    public IReadOnlyList<Account> Accounts => _accounts;

    public IReadOnlyList<WsClient> Clients => _clients;

    public Account Account(int index) => _accounts[index];

    public WsClient Client(int index) => _clients[index];

    public static string DmId(Account a, Account b) => $"dm-{Math.Min(a.UserId, b.UserId)}-{Math.Max(a.UserId, b.UserId)}";

    public static async Task<Party> ConnectAsync(VorcallFactory factory, params string[] prefixes)
    {
        var accounts = new List<Account>(prefixes.Length);
        foreach (var prefix in prefixes)
        {
            accounts.Add(await Infrastructure.Accounts.RegisterAsync(factory, prefix));
        }

        var clients = new List<WsClient>(prefixes.Length);
        var party = new Party(accounts, clients);
        try
        {
            foreach (var account in accounts)
            {
                var client = await WsClient.ConnectAsync(factory, account);
                foreach (var earlier in clients)
                {
                    await earlier.ExpectMemberJoinedAsync(Session.General, account);
                }

                clients.Add(client);
            }
        }
        catch
        {
            await party.DisposeAsync();
            throw;
        }

        return party;
    }

    // Swaps one member's socket for a reconnected one; the old socket is already closed.
    public async Task ReconnectAsync(VorcallFactory factory, int index)
    {
        var replacement = await WsClient.ReconnectAsync(factory, _accounts[index], _clients[index]);
        await _clients[index].DisposeAsync();
        _clients[index] = replacement;
    }

    // A public room created by the first member, with the frames it causes consumed: RoomState
    // on the creator, RoomUpdated on every socket. The name slugs to a predictable id.
    public async Task<(string Name, string Id)> CreateRoomAsync()
    {
        var token = Names.Token();
        var name = $"Team  Chat_2 {token}";
        var id = $"team-chat-2-{token}";
        var creator = _clients[0];
        await creator.SendAsync(Frames.CreateRoom(name));
        var state = (await creator.ExpectAsync(Kind.RoomState)).RoomState;
        Assert.Equal(id, state.RoomId);
        foreach (var client in _clients)
        {
            var room = (await client.ExpectAsync(Kind.RoomUpdated)).RoomUpdated.Room;
            Assert.Equal(id, room.RoomId);
        }

        return (name, id);
    }

    // Opens the DM between two members from the caller's socket and consumes what both of them
    // receive: RoomState then RoomUpdated for the DM.
    public async Task<string> OpenDmAsync(int caller, int other)
    {
        var dmId = DmId(_accounts[caller], _accounts[other]);
        var members = new[] { _accounts[caller].UserId, _accounts[other].UserId }.Order().ToArray();
        await _clients[caller].SendAsync(Frames.OpenDm(_accounts[other].UserId));
        foreach (var index in new[] { caller, other })
        {
            var client = _clients[index];
            var state = (await client.ExpectAsync(Kind.RoomState)).RoomState;
            Assert.Equal(dmId, state.RoomId);
            var room = (await client.ExpectAsync(Kind.RoomUpdated)).RoomUpdated.Room;
            Assert.Equal(dmId, room.RoomId);
            Assert.Equal(RoomKind.Dm, room.Kind);
            Assert.Equal(string.Empty, room.Name);
            Assert.Equal(members, room.MemberIds.Order().ToArray());
        }

        return dmId;
    }

    // Every socket receives one ChatMessage with that text; returns the id they all carry.
    public async Task<long> ExpectMessageEverywhereAsync(string text, string roomId = Session.General)
    {
        long id = 0;
        foreach (var client in _clients)
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(text, message.Text);
            Assert.Equal(roomId, message.RoomId);
            Assert.True(id == 0 || id == message.Id, $"{client.Label}: message id {message.Id} differs from {id}");
            id = message.Id;
        }

        return id;
    }

    // One frame of that kind on every socket, in party order.
    public async Task<ServerFrame[]> ExpectEverywhereAsync(Kind kind)
    {
        var frames = new ServerFrame[_clients.Count];
        for (var i = 0; i < _clients.Count; i++)
        {
            frames[i] = await _clients[i].ExpectAsync(kind);
        }

        return frames;
    }

    public async ValueTask DisposeAsync()
    {
        foreach (var client in _clients)
        {
            await client.DisposeAsync();
        }
    }
}
