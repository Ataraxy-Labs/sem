"""Data models with inheritance and super() calls."""


class BaseModel:
    def __init__(self, name):
        self.name = name

    def serialize(self):
        return {"name": self.name}


class UserModel(BaseModel):
    def __init__(self, name, email):
        super().__init__(name)
        self.email = email

    def serialize(self):
        base = super().serialize()
        base["email"] = self.email
        return base
