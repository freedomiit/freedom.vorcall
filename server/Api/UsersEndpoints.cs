using Vorcall.Server.Auth;

namespace Vorcall.Server.Api;

public static class UsersEndpoints
{
    public static void Map(WebApplication app) => app.MapGet("/api/users", ListAsync).RequireAuthorization();

    // GET /api/users -> UserList as application/x-protobuf, ordered case-insensitively.
    private static async Task<IResult> ListAsync(AccountService accounts) => ProtobufBody.Proto(await accounts.ListUsersAsync());
}
