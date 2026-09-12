using System;
using Microsoft.EntityFrameworkCore.Migrations;

#nullable disable

namespace Vorcall.Server.Migrations
{
    /// <inheritdoc />
    public partial class AddDisabledAndRevoked : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.AddColumn<DateTime>(
                name: "disabled_at",
                table: "users",
                type: "timestamp with time zone",
                nullable: true);

            migrationBuilder.AddColumn<DateTime>(
                name: "revoked_at",
                table: "invites",
                type: "timestamp with time zone",
                nullable: true);
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.DropColumn(
                name: "disabled_at",
                table: "users");

            migrationBuilder.DropColumn(
                name: "revoked_at",
                table: "invites");
        }
    }
}
