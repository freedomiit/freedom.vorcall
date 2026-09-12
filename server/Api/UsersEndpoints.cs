using Vorcall.Server.Chat;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Api;

public static class UsersEndpoints
{
    public static void Map(WebApplication app) => app.MapGet("/api/users", ListMembers).RequireAuthorization();

    // GET /api/users -> MemberList as application/x-protobuf, ordered case-insensitively. Read from
    // the registry's mirror rather than the database: it is the same set every snapshot carries,
    // banned accounts already dropped.
    private static IResult ListMembers(ConnectionRegistry registry)
    {
        var list = new MemberList();
        list.Members.AddRange(registry.Profiles().OrderBy(profile => profile.Username, StringComparer.OrdinalIgnoreCase));
        return ProtobufBody.Proto(list);
    }
}
