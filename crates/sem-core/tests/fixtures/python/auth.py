class User:
    def __init__(self, name, email):
        self.name = name
        self.email = email

    def validate(self):
        return len(self.name) > 0 and "@" in self.email

    def to_dict(self):
        return {"name": self.name, "email": self.email}


class Admin(User):
    def __init__(self, name, email, role):
        super().__init__(name, email)
        self.role = role

    def validate(self):
        return super().validate() and self.role in ["admin", "superadmin"]

    def get_permissions(self):
        if self.role == "superadmin":
            return ["read", "write", "delete", "manage"]
        return ["read", "write"]
